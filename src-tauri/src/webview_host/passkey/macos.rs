//! macOS 实现：WKScriptMessageHandlerWithReply 接收页面请求，经
//! AuthenticationServices 的浏览器通行密钥接口完成注册 / 登录。
//!
//! 所有 AppKit / WebKit / AuthenticationServices 对象只在主线程访问；
//! 系统授权回调可能在任意线程触发，经主队列切回后再继续。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use block2::{DynBlock, RcBlock};
use dispatch2::{DispatchQueue, MainThreadBound};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::NSWindow;
use objc2_authentication_services::{
    ASAuthorization, ASAuthorizationController, ASAuthorizationControllerDelegate,
    ASAuthorizationControllerPresentationContextProviding,
    ASAuthorizationPlatformPublicKeyCredentialAssertion,
    ASAuthorizationPlatformPublicKeyCredentialDescriptor,
    ASAuthorizationPlatformPublicKeyCredentialProvider,
    ASAuthorizationPlatformPublicKeyCredentialRegistration,
    ASAuthorizationPublicKeyCredentialAssertion,
    ASAuthorizationPublicKeyCredentialAssertionRequest,
    ASAuthorizationPublicKeyCredentialAttestationKindDirect,
    ASAuthorizationPublicKeyCredentialAttestationKindIndirect,
    ASAuthorizationPublicKeyCredentialAttestationKindNone,
    ASAuthorizationPublicKeyCredentialRegistration,
    ASAuthorizationPublicKeyCredentialRegistrationRequest,
    ASAuthorizationPublicKeyCredentialUserVerificationPreference,
    ASAuthorizationPublicKeyCredentialUserVerificationPreferenceDiscouraged,
    ASAuthorizationPublicKeyCredentialUserVerificationPreferencePreferred,
    ASAuthorizationPublicKeyCredentialUserVerificationPreferenceRequired, ASAuthorizationRequest,
    ASAuthorizationWebBrowserPlatformPublicKeyCredentialAssertionRequest,
    ASAuthorizationWebBrowserPlatformPublicKeyCredentialProvider,
    ASAuthorizationWebBrowserPlatformPublicKeyCredentialRegistrationRequest,
    ASAuthorizationWebBrowserPublicKeyCredentialManager,
    ASAuthorizationWebBrowserPublicKeyCredentialManagerAuthorizationState as AuthState,
    ASPresentationAnchor, ASPublicKeyCredential, ASPublicKeyCredentialClientData,
};
use objc2_foundation::{NSArray, NSData, NSError, NSObjectProtocol, NSString};
use objc2_web_kit::{
    WKContentWorld, WKScriptMessage, WKScriptMessageHandlerWithReply, WKUserContentController,
};
use tracing::{debug, warn};

use super::protocol::{
    self, Attestation, FrameOrigin, IncomingMessage, PasskeyError, PreparedKind, PreparedRequest,
    UserVerification,
};
use super::{ENTITLEMENT, MESSAGE_HANDLER_NAME};

type ReplyBlock = RcBlock<dyn Fn(*mut AnyObject, *mut NSString)>;

// ── 可用性门控 ──

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecTaskCreateFromSelf(allocator: *const c_void) -> *mut c_void;
    fn SecTaskCopyValueForEntitlement(
        task: *mut c_void,
        entitlement: *const c_void,
        error: *mut *mut c_void,
    ) -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: *const c_void;
    fn CFRelease(cf: *const c_void);
}

/// 读取当前进程签名中的布尔型权限。
fn has_entitlement(name: &str) -> bool {
    let key = NSString::from_str(name);
    // SAFETY: SecTask/CF 均为线程安全的 C 接口；NSString 与 CFString 免费桥接；
    // 返回的 Create/Copy 对象按 CF 规则释放。
    unsafe {
        let task = SecTaskCreateFromSelf(std::ptr::null());
        if task.is_null() {
            return false;
        }
        let value = SecTaskCopyValueForEntitlement(
            task,
            Retained::as_ptr(&key).cast(),
            std::ptr::null_mut(),
        );
        let granted = !value.is_null() && value == kCFBooleanTrue;
        if !value.is_null() {
            CFRelease(value);
        }
        CFRelease(task);
        granted
    }
}

pub(super) fn is_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let available = objc2::available!(macos = 13.5) && has_entitlement(ENTITLEMENT);
        debug!(available, "浏览器通行密钥桥接可用性");
        available
    })
}

fn credential_manager() -> &'static ASAuthorizationWebBrowserPublicKeyCredentialManager {
    static MANAGER: OnceLock<Retained<ASAuthorizationWebBrowserPublicKeyCredentialManager>> =
        OnceLock::new();
    // SAFETY: init 无前置条件；该类型声明为 Send + Sync。
    MANAGER.get_or_init(|| unsafe {
        ASAuthorizationWebBrowserPublicKeyCredentialManager::init(
            ASAuthorizationWebBrowserPublicKeyCredentialManager::alloc(),
        )
    })
}

// ── 注册消息处理器 ──

pub(super) fn attach(webview: &tauri::Webview<tauri::Wry>) {
    let result = webview.with_webview(|platform| {
        let Some(mtm) = MainThreadMarker::new() else {
            warn!("通行密钥处理器注册不在主线程，已跳过");
            return;
        };
        let controller = platform.controller().cast::<WKUserContentController>();
        if controller.is_null() {
            return;
        }
        let handler = MessageHandler::new(mtm);
        // SAFETY: controller 来自 wry，生命周期与 WebView 一致；主线程调用。
        // 每个 WebView 只注册一次同名处理器。
        unsafe {
            (*controller).addScriptMessageHandlerWithReply_contentWorld_name(
                ProtocolObject::from_ref(&*handler),
                &WKContentWorld::pageWorld(mtm),
                &NSString::from_str(MESSAGE_HANDLER_NAME),
            );
        }
    });
    if let Err(error) = result {
        warn!(%error, "注册通行密钥处理器失败");
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TiangongPasskeyMessageHandler"]
    struct MessageHandler;

    unsafe impl NSObjectProtocol for MessageHandler {}

    unsafe impl WKScriptMessageHandlerWithReply for MessageHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:replyHandler:))]
        fn did_receive(
            &self,
            _controller: &WKUserContentController,
            message: &WKScriptMessage,
            reply: &DynBlock<dyn Fn(*mut AnyObject, *mut NSString)>,
        ) {
            handle_message(self.mtm(), message, reply.copy());
        }
    }
);

impl MessageHandler {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(());
        // SAFETY: NSObject 的指定初始化方法。
        unsafe { msg_send![super(this), init] }
    }
}

// ── 回复页面 ──

fn reply_ok(reply: &ReplyBlock, json: &str) {
    let text = NSString::from_str(json);
    reply.call((
        Retained::as_ptr(&text) as *mut AnyObject,
        std::ptr::null_mut(),
    ));
}

fn reply_err(reply: &ReplyBlock, error: &PasskeyError) {
    let text = NSString::from_str(&error.to_reply());
    reply.call((
        std::ptr::null_mut(),
        Retained::as_ptr(&text) as *mut NSString,
    ));
}

fn reply_empty(reply: &ReplyBlock) {
    reply.call((std::ptr::null_mut(), std::ptr::null_mut()));
}

// ── 请求处理 ──

/// 进行中的系统请求，键为 `<WebView 指针>:<页面 requestId>`，供取消使用。
struct ActiveRequest {
    controller: Retained<ASAuthorizationController>,
    _delegate: Retained<RequestDelegate>,
}

thread_local! {
    static ACTIVE: RefCell<HashMap<String, ActiveRequest>> = RefCell::new(HashMap::new());
}

/// 已校验、等待系统授权结果的请求。
struct PendingRequest {
    key: String,
    prepared: PreparedRequest,
    reply: ReplyBlock,
    anchor: Retained<NSWindow>,
}

fn handle_message(mtm: MainThreadMarker, message: &WKScriptMessage, reply: ReplyBlock) {
    // SAFETY: 以下均为主线程上对 WebKit 只读属性的访问。
    let (body, frame, webview) =
        unsafe { (message.body(), message.frameInfo(), message.webView()) };
    let Ok(body) = body.downcast::<NSString>() else {
        reply_err(&reply, &PasskeyError::Type("无效的请求".into()));
        return;
    };
    let incoming = match protocol::parse_message(&body.to_string()) {
        Ok(incoming) => incoming,
        Err(error) => {
            reply_err(&reply, &error);
            return;
        }
    };
    let webview_key = webview
        .as_ref()
        .map(|w| Retained::as_ptr(w) as usize)
        .unwrap_or_default();
    let request_key = |id: &str| format!("{webview_key:x}:{id}");

    let (key, prepared) = match incoming {
        IncomingMessage::Cancel { request_id } => {
            cancel(&request_key(&request_id));
            reply_empty(&reply);
            return;
        }
        IncomingMessage::Create {
            request_id,
            options,
        } => (request_key(&request_id), Kind::Create(options)),
        IncomingMessage::Get {
            request_id,
            options,
        } => (request_key(&request_id), Kind::Get(options)),
    };

    // SAFETY: 主线程读取 frame 信息；来源取自 WebKit 而非页面自报。
    let (is_main_frame, scheme, host, port) = unsafe {
        let origin = frame.securityOrigin();
        (
            frame.isMainFrame(),
            origin.protocol().to_string(),
            origin.host().to_string(),
            origin.port(),
        )
    };
    if !is_main_frame {
        reply_err(
            &reply,
            &PasskeyError::NotAllowed("内置浏览器暂不支持在内嵌框架中使用通行密钥".into()),
        );
        return;
    }
    let frame_origin = FrameOrigin {
        scheme: &scheme,
        host: &host,
        port: u16::try_from(port).unwrap_or(0),
    };
    let prepared = match &prepared {
        Kind::Create(options) => protocol::prepare_create(frame_origin, options),
        Kind::Get(options) => protocol::prepare_get(frame_origin, options),
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            reply_err(&reply, &error);
            return;
        }
    };
    let Some(anchor) = webview.and_then(|w| w.window()) else {
        reply_err(
            &reply,
            &PasskeyError::NotAllowed("浏览器窗口不可用，无法显示通行密钥对话框".into()),
        );
        return;
    };
    let pending = PendingRequest {
        key,
        prepared,
        reply,
        anchor,
    };

    let manager = credential_manager();
    // SAFETY: 只读属性，在主线程读取。
    match unsafe { manager.authorizationStateForPlatformCredentials() } {
        AuthState::Authorized => perform(mtm, pending),
        AuthState::NotDetermined => request_authorization(manager, pending, mtm),
        _ => reply_err(
            &pending.reply,
            &PasskeyError::NotAllowed(
                "天工未获准使用通行密钥，请在「系统设置 > 隐私与安全性 > 访问网页浏览器的通行密钥」中开启"
                    .into(),
            ),
        ),
    }
}

enum Kind {
    Create(protocol::CreateOptions),
    Get(protocol::GetOptions),
}

/// 页面第一次需要通行密钥时向用户申请系统授权，结果切回主线程继续。
fn request_authorization(
    manager: &ASAuthorizationWebBrowserPublicKeyCredentialManager,
    pending: PendingRequest,
    mtm: MainThreadMarker,
) {
    let slot = Mutex::new(Some(MainThreadBound::new(pending, mtm)));
    let completion = RcBlock::new(move |state: AuthState| {
        let Some(bound) = slot.lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        DispatchQueue::main().exec_async(move || {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let pending = bound.into_inner(mtm);
            if state == AuthState::Authorized {
                perform(mtm, pending);
            } else {
                reply_err(
                    &pending.reply,
                    &PasskeyError::NotAllowed("用户未允许天工使用通行密钥".into()),
                );
            }
        });
    });
    // SAFETY: completion 为堆上 block，系统按需持有。
    unsafe { manager.requestAuthorizationForPublicKeyCredentials(&completion) };
}

fn cancel(key: &str) {
    let controller = ACTIVE.with(|active| active.borrow().get(key).map(|r| r.controller.clone()));
    if let Some(controller) = controller {
        // SAFETY: 主线程调用；系统随后以取消错误回调 delegate 并清理。
        unsafe { controller.cancel() };
    }
}

fn user_verification_preference(
    value: UserVerification,
) -> Option<&'static ASAuthorizationPublicKeyCredentialUserVerificationPreference> {
    // SAFETY: 读取框架导出的常量。
    unsafe {
        match value {
            UserVerification::Required => {
                ASAuthorizationPublicKeyCredentialUserVerificationPreferenceRequired
            }
            UserVerification::Preferred => {
                ASAuthorizationPublicKeyCredentialUserVerificationPreferencePreferred
            }
            UserVerification::Discouraged => {
                ASAuthorizationPublicKeyCredentialUserVerificationPreferenceDiscouraged
            }
        }
    }
}

fn descriptors(
    ids: &[Vec<u8>],
) -> Retained<NSArray<ASAuthorizationPlatformPublicKeyCredentialDescriptor>> {
    let list: Vec<_> = ids
        .iter()
        .map(|id| {
            // SAFETY: 指定初始化方法。
            unsafe {
                ASAuthorizationPlatformPublicKeyCredentialDescriptor::initWithCredentialID(
                    ASAuthorizationPlatformPublicKeyCredentialDescriptor::alloc(),
                    &NSData::with_bytes(id),
                )
            }
        })
        .collect();
    NSArray::from_retained_slice(&list)
}

fn build_request(
    prepared: &PreparedRequest,
) -> Result<Retained<ASAuthorizationRequest>, PasskeyError> {
    // SAFETY: 均为 AuthenticationServices 的构造与属性设置，主线程调用。
    unsafe {
        let client_data = ASPublicKeyCredentialClientData::initWithChallenge_origin(
            ASPublicKeyCredentialClientData::alloc(),
            &NSData::with_bytes(&prepared.challenge),
            &NSString::from_str(&prepared.origin),
        );
        let provider =
            ASAuthorizationPlatformPublicKeyCredentialProvider::initWithRelyingPartyIdentifier(
                ASAuthorizationPlatformPublicKeyCredentialProvider::alloc(),
                &NSString::from_str(&prepared.rp_id),
            );
        match &prepared.kind {
            PreparedKind::Create(create) => {
                let name = [&create.user_name, &create.display_name]
                    .into_iter()
                    .find(|s| !s.is_empty())
                    .ok_or_else(|| PasskeyError::Type("user.name 不能为空".into()))?;
                let request = provider
                    .createCredentialRegistrationRequestWithClientData_name_userID(
                        &client_data,
                        &NSString::from_str(name),
                        &NSData::with_bytes(&create.user_id),
                    );
                if !create.display_name.is_empty() {
                    request.setDisplayName(Some(&NSString::from_str(&create.display_name)));
                }
                if let Some(pref) = user_verification_preference(create.user_verification) {
                    request.setUserVerificationPreference(pref);
                }
                let attestation = match create.attestation {
                    Attestation::None => ASAuthorizationPublicKeyCredentialAttestationKindNone,
                    Attestation::Indirect => {
                        ASAuthorizationPublicKeyCredentialAttestationKindIndirect
                    }
                    Attestation::Direct => ASAuthorizationPublicKeyCredentialAttestationKindDirect,
                };
                if let Some(attestation) = attestation {
                    request.setAttestationPreference(attestation);
                }
                if !create.exclude_credentials.is_empty() {
                    request.setExcludedCredentials(Some(&descriptors(&create.exclude_credentials)));
                }
                request.setShouldShowHybridTransport(true);
                Ok(request.into_super())
            }
            PreparedKind::Get(get) => {
                let request = provider.createCredentialAssertionRequestWithClientData(&client_data);
                if !get.allow_credentials.is_empty() {
                    request.setAllowedCredentials(&descriptors(&get.allow_credentials));
                }
                if let Some(pref) = user_verification_preference(get.user_verification) {
                    request.setUserVerificationPreference(pref);
                }
                request.setShouldShowHybridTransport(true);
                Ok(request.into_super())
            }
        }
    }
}

fn perform(mtm: MainThreadMarker, pending: PendingRequest) {
    let request = match build_request(&pending.prepared) {
        Ok(request) => request,
        Err(error) => {
            reply_err(&pending.reply, &error);
            return;
        }
    };
    let key = pending.key.clone();
    // 同一页面重复使用 requestId（异常情况）时先取消旧请求
    cancel(&key);
    let delegate = RequestDelegate::new(mtm, pending);
    // SAFETY: 主线程构造控制器并发起请求；delegate 与控制器由 ACTIVE 持有至回调结束。
    let controller = unsafe {
        let controller = ASAuthorizationController::initWithAuthorizationRequests(
            ASAuthorizationController::alloc(),
            &NSArray::from_retained_slice(&[request]),
        );
        controller.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        controller.setPresentationContextProvider(Some(ProtocolObject::from_ref(&*delegate)));
        controller
    };
    ACTIVE.with(|active| {
        active.borrow_mut().insert(
            key,
            ActiveRequest {
                controller: controller.clone(),
                _delegate: delegate,
            },
        );
    });
    // SAFETY: 主线程调用。
    unsafe { controller.performRequests() };
}

struct RequestIvars {
    key: String,
    reply: RefCell<Option<ReplyBlock>>,
    anchor: Retained<NSWindow>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TiangongPasskeyRequestDelegate"]
    #[ivars = RequestIvars]
    struct RequestDelegate;

    unsafe impl NSObjectProtocol for RequestDelegate {}

    unsafe impl ASAuthorizationControllerDelegate for RequestDelegate {
        #[unsafe(method(authorizationController:didCompleteWithAuthorization:))]
        fn did_complete(
            &self,
            _controller: &ASAuthorizationController,
            authorization: &ASAuthorization,
        ) {
            let result = credential_json(authorization);
            self.finish(result);
        }

        #[unsafe(method(authorizationController:didCompleteWithError:))]
        fn did_fail(&self, _controller: &ASAuthorizationController, error: &NSError) {
            debug!(code = error.code(), "通行密钥请求未完成");
            self.finish(Err(protocol::map_authorization_error(error.code())));
        }
    }

    unsafe impl ASAuthorizationControllerPresentationContextProviding for RequestDelegate {
        #[unsafe(method_id(presentationAnchorForAuthorizationController:))]
        fn presentation_anchor(
            &self,
            _controller: &ASAuthorizationController,
        ) -> Retained<ASPresentationAnchor> {
            Retained::into_super(Retained::into_super(self.ivars().anchor.clone()))
        }
    }
);

impl RequestDelegate {
    fn new(mtm: MainThreadMarker, pending: PendingRequest) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(RequestIvars {
            key: pending.key,
            reply: RefCell::new(Some(pending.reply)),
            anchor: pending.anchor,
        });
        // SAFETY: NSObject 的指定初始化方法。
        unsafe { msg_send![super(this), init] }
    }

    fn finish(&self, result: Result<String, PasskeyError>) {
        if let Some(reply) = self.ivars().reply.borrow_mut().take() {
            match result {
                Ok(json) => reply_ok(&reply, &json),
                Err(error) => reply_err(&reply, &error),
            }
        }
        // 回调仍在执行，延后到下一轮主循环再释放控制器与本对象。
        let key = self.ivars().key.clone();
        DispatchQueue::main().exec_async(move || {
            ACTIVE.with(|active| {
                active.borrow_mut().remove(&key);
            });
        });
    }
}

fn credential_json(authorization: &ASAuthorization) -> Result<String, PasskeyError> {
    // SAFETY: 主线程读取系统返回的凭据对象。
    unsafe {
        let credential = authorization.credential();
        let object: &AnyObject = (*credential).as_ref();
        if let Some(registration) =
            object.downcast_ref::<ASAuthorizationPlatformPublicKeyCredentialRegistration>()
        {
            let attestation = registration
                .rawAttestationObject()
                .ok_or_else(|| PasskeyError::Unknown("系统未返回 attestationObject".into()))?;
            let value = protocol::registration_response(
                &registration.credentialID().to_vec(),
                &registration.rawClientDataJSON().to_vec(),
                &attestation.to_vec(),
            )?;
            return Ok(value.to_string());
        }
        if let Some(assertion) =
            object.downcast_ref::<ASAuthorizationPlatformPublicKeyCredentialAssertion>()
        {
            let value = protocol::assertion_response(
                &assertion.credentialID().to_vec(),
                &assertion.rawClientDataJSON().to_vec(),
                &assertion.rawAuthenticatorData().to_vec(),
                &assertion.signature().to_vec(),
                &assertion.userID().to_vec(),
            );
            return Ok(value.to_string());
        }
    }
    Err(PasskeyError::Unknown("系统返回了未知类型的凭据".into()))
}
