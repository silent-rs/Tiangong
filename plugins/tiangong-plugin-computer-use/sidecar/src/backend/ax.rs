//! macOS AXUIElement C API 的安全封装。
//!
//! 直接链接 ApplicationServices 框架的 AXUIElement 函数，用 core-foundation
//! crate 处理 CFString/CFArray/CFBoolean/CFNumber 返回值，手动管理引用计数。
//!
//! 设计要点：
//! - `AxElement` 持有一个 `AXUIElementRef` 并在 Drop 时 `CFRelease`。
//! - 所有读取操作返回 `Result<T, AxError>`，错误映射自 `AXError`。
//! - 不暴露原始指针，调用方只接触安全类型。

use std::ffi::c_void;

use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFRelease, CFRetain, CFTypeRef, TCFType};
use core_foundation::boolean::{CFBoolean, CFBooleanRef};
use core_foundation::string::{CFString, CFStringRef};

/// AXError 值（仅取常用项）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum AxError {
    Success = 0,
    Failure = -25200,
    IllegalArgument = -25201,
    InvalidUIElement = -25202,
    CannotComplete = -25204,
    AttributeUnsupported = -25205,
    ActionUnsupported = -25206,
    NotImplemented = -25208,
    NotEnoughPrecision = -25211,
    Other = -25299,
}

impl AxError {
    pub fn is_success(self) -> bool {
        matches!(self, Self::Success)
    }

    /// 由原始 AXError（i32）构造。
    fn from_raw(code: i32) -> Self {
        match code {
            0 => Self::Success,
            -25200 => Self::Failure,
            -25201 => Self::IllegalArgument,
            -25202 => Self::InvalidUIElement,
            -25204 => Self::CannotComplete,
            -25205 => Self::AttributeUnsupported,
            -25206 => Self::ActionUnsupported,
            -25208 => Self::NotImplemented,
            -25211 => Self::NotEnoughPrecision,
            _ => Self::Other,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::Success => "成功",
            Self::Failure => "AX 调用失败",
            Self::IllegalArgument => "非法参数",
            Self::InvalidUIElement => "控件引用无效",
            Self::CannotComplete => "无法完成（可能未授权或进程不可达）",
            Self::AttributeUnsupported => "控件不支持该属性",
            Self::ActionUnsupported => "控件不支持该动作",
            Self::NotImplemented => "未实现",
            Self::NotEnoughPrecision => "精度不足",
            Self::Other => "未知 AX 错误",
        }
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyElementAtPosition(
        application: AXUIElementRef,
        x: f64,
        y: f64,
        element: *mut AXUIElementRef,
    ) -> i32;
    fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> i32;
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCopyAttributeNames(element: AXUIElementRef, names: *mut CFArrayRef) -> i32;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> i32;
    fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> i32;
    fn AXValueGetType(value: AXValueRef) -> u32;
    fn AXValueGetValue(value: AXValueRef, the_type: u32, value_ptr: *mut c_void) -> u8;
    fn AXValueCreate(the_type: u32, value_ptr: *const c_void) -> AXValueRef;
    fn AXUIElementIsAttributeSettable(
        element: AXUIElementRef,
        attribute: CFStringRef,
        settable: *mut u8,
    ) -> i32;
}

/// AXValue 类型常量（与 HIServices AXValue.h 对齐）。
const AX_VALUE_TYPE_CG_POINT: u32 = 1;
const AX_VALUE_TYPE_CG_SIZE: u32 = 2;

// CoreFoundation 类型判断（用于按真实类型解释 CFTypeRef，避免类型误判）。
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: *const c_void, the_type: u32, value_ptr: *mut c_void) -> u8;
}

/// CFNumber 类型常量（kCFNumberIntType=9, kCFNumberFloat64Type=13, kCFNumberDoubleType=13）。
const CF_NUMBER_FLOAT64_TYPE: u32 = 13;
const CF_NUMBER_INT_TYPE: u32 = 9;

/// CGPoint（64 位下 CGFloat = f64）。
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

/// CGSize。
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

/// 屏幕边界（与 protocol Bounds 对齐）。
#[derive(Default, Clone, Copy)]
pub struct AxBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// AXUIElementRef 即 CFTypeRef。
pub type AXUIElementRef = *const c_void;
/// AXValueRef 即 CFTypeRef。
pub type AXValueRef = *const c_void;

/// 持有 AXUIElement 引用的 RAII 包装。
pub struct AxElement {
    raw: AXUIElementRef,
}

impl Clone for AxElement {
    fn clone(&self) -> Self {
        // SAFETY：对非空 CF 对象增引用计数，产生一个新的独立引用。
        let raw = if self.raw.is_null() {
            self.raw
        } else {
            unsafe { CFRetain(self.raw as CFTypeRef) }
        };
        Self { raw }
    }
}

// SAFETY：AXUIElement 是 CoreFoundation 引用计数对象，可跨线程传递与访问。
// Apple 的辅助功能 API 文档明确属性读取是线程安全的。
unsafe impl Send for AxElement {}
unsafe impl Sync for AxElement {}

impl AxElement {
    /// 通过进程号创建应用根元素。
    pub fn for_application(pid: i32) -> Self {
        // SAFETY：AXUIElementCreateApplication 返回一个新的引用（caller 必须 release）。
        let raw = unsafe { AXUIElementCreateApplication(pid) };
        Self { raw }
    }

    /// 由原始引用构造，并 retain（用于从数组中取出的元素）。
    pub fn from_retained(raw: AXUIElementRef) -> Option<Self> {
        if raw.is_null() {
            return None;
        }
        Some(Self { raw })
    }

    pub fn raw(&self) -> AXUIElementRef {
        self.raw
    }

    /// 读取控件的属性名列表。
    pub fn attribute_names(&self) -> Result<Vec<String>, AxError> {
        let mut array: CFArrayRef = std::ptr::null();
        let code = unsafe { AXUIElementCopyAttributeNames(self.raw, &mut array) };
        let err = AxError::from_raw(code);
        if !err.is_success() || array.is_null() {
            return Err(err);
        }
        // SAFETY：array 由 Copy API 返回，caller owned，用 CFArray 包裹后 Drop 时释放。
        let cf_array = unsafe { CFArray::<*const c_void>::wrap_under_create_rule(array) };
        let range = core_foundation::base::CFRange::init(0, cf_array.len());
        let ptrs = cf_array.get_values(range);
        let mut names = Vec::with_capacity(ptrs.len());
        for ptr in ptrs {
            if let Some(s) = cf_type_as_string(ptr) {
                names.push(s);
            }
        }
        Ok(names)
    }

    /// 读取字符串属性。
    /// 整体用 ax_catch 包裹：某些控件的属性值类型异常，CFString 转换会抛 NSException。
    pub fn string_attribute(&self, name: &str) -> Option<String> {
        let value = self.copy_attribute_value(name)?;
        ax_catch(|| cf_type_as_string(value)).flatten()
    }

    /// 读取布尔属性。
    /// 整体用 ax_catch 包裹：类型不匹配时 CFBoolean 转换会抛 NSException。
    pub fn bool_attribute(&self, name: &str) -> Option<bool> {
        let value = self.copy_attribute_value(name)?;
        ax_catch(|| unsafe {
            let cf_bool = CFBoolean::wrap_under_create_rule(value as CFBooleanRef);
            bool::from(cf_bool)
        })
    }

    /// 读取控件边界（合并 AXPosition 与 AXSize）。
    /// 读取失败时返回默认（全零）边界，不抛错，避免单个控件拖垮整棵树。
    pub fn bounds(&self) -> AxBounds {
        let position = self.read_point("AXPosition");
        let size = self.read_size("AXSize");
        AxBounds {
            x: position.map(|p| p.x).unwrap_or_default(),
            y: position.map(|p| p.y).unwrap_or_default(),
            width: size.map(|s| s.width).unwrap_or_default(),
            height: size.map(|s| s.height).unwrap_or_default(),
        }
    }

    /// 读取 CGPoint 属性。
    fn read_point(&self, name: &str) -> Option<CGPoint> {
        let value = self.copy_attribute_value(name)?;
        // SAFETY：value 为 caller-owned CFTypeRef，作为 AXValue 解释后释放。
        // 用 ax_catch 包裹，防止异常控件抛 NSException。
        ax_catch(|| unsafe {
            let ax_value = value as AXValueRef;
            if AXValueGetType(ax_value) != AX_VALUE_TYPE_CG_POINT {
                CFRelease(value);
                return None;
            }
            let mut point = CGPoint::default();
            let ok = AXValueGetValue(
                ax_value,
                AX_VALUE_TYPE_CG_POINT,
                &mut point as *mut CGPoint as *mut c_void,
            );
            CFRelease(value);
            if ok != 0 { Some(point) } else { None }
        })
        .flatten()
    }

    /// 读取 CGSize 属性。
    fn read_size(&self, name: &str) -> Option<CGSize> {
        let value = self.copy_attribute_value(name)?;
        ax_catch(|| unsafe {
            let ax_value = value as AXValueRef;
            if AXValueGetType(ax_value) != AX_VALUE_TYPE_CG_SIZE {
                CFRelease(value);
                return None;
            }
            let mut size = CGSize::default();
            let ok = AXValueGetValue(
                ax_value,
                AX_VALUE_TYPE_CG_SIZE,
                &mut size as *mut CGSize as *mut c_void,
            );
            CFRelease(value);
            if ok != 0 { Some(size) } else { None }
        })
        .flatten()
    }

    /// 读取子控件数组（AXChildren）。
    pub fn children(&self) -> Result<Vec<AxElement>, AxError> {
        self.elements_attribute("AXChildren")
    }

    /// 读取元素数组属性（AXChildren / AXWindows 等）。
    pub fn elements_attribute(&self, name: &str) -> Result<Vec<AxElement>, AxError> {
        let value = match self.copy_attribute_value(name) {
            Some(v) => v,
            None => return Err(AxError::AttributeUnsupported),
        };
        if value.is_null() {
            return Ok(Vec::new());
        }
        // SAFETY：value 为 caller-owned CFTypeRef，这里作为 CFArray 解释。
        // 数组解析用 ax_catch 包裹，防止异常控件抛 NSException。
        let children = ax_catch(|| unsafe {
            let cf_array = CFArray::<*const c_void>::wrap_under_create_rule(value as CFArrayRef);
            let range = core_foundation::base::CFRange::init(0, cf_array.len());
            let ptrs = cf_array.get_values(range);
            let mut children = Vec::with_capacity(ptrs.len());
            for child_ptr in ptrs {
                // 数组中的元素是未 retain 的引用，需要 retain 后再用。
                if let Some(child) = Self::from_retained(retain(child_ptr)) {
                    children.push(child);
                }
            }
            children
        });
        Ok(children.unwrap_or_default())
    }

    /// 执行动作（如 AXPress）。
    pub fn perform_action(&self, action: &str) -> Result<(), AxError> {
        let raw = self.raw;
        let cf_action = CFString::new(action);
        match ax_catch(|| unsafe { AXUIElementPerformAction(raw, cf_action.as_concrete_TypeRef()) })
        {
            Some(code) => {
                let err = AxError::from_raw(code);
                if err.is_success() { Ok(()) } else { Err(err) }
            }
            None => Err(AxError::Failure),
        }
    }

    /// 设置字符串属性（如 AXValue）。
    pub fn set_string_attribute(&self, name: &str, value: &str) -> Result<(), AxError> {
        let raw = self.raw;
        let cf_value = CFString::new(value);
        let cf_attr = CFString::new(name);
        match ax_catch(|| unsafe {
            AXUIElementSetAttributeValue(
                raw,
                cf_attr.as_concrete_TypeRef(),
                cf_value.as_concrete_TypeRef() as CFTypeRef,
            )
        }) {
            Some(code) => {
                let err = AxError::from_raw(code);
                if err.is_success() { Ok(()) } else { Err(err) }
            }
            None => Err(AxError::Failure),
        }
    }

    /// 设置布尔属性（如设置 AXFocused 为 true 以聚焦控件）。
    pub fn set_bool_attribute(&self, name: &str, value: bool) -> Result<(), AxError> {
        let raw = self.raw;
        let cf_value = if value {
            CFBoolean::true_value()
        } else {
            CFBoolean::false_value()
        };
        let cf_attr = CFString::new(name);
        match ax_catch(|| unsafe {
            AXUIElementSetAttributeValue(
                raw,
                cf_attr.as_concrete_TypeRef(),
                cf_value.as_concrete_TypeRef() as CFTypeRef,
            )
        }) {
            Some(code) => {
                let err = AxError::from_raw(code);
                if err.is_success() { Ok(()) } else { Err(err) }
            }
            None => Err(AxError::Failure),
        }
    }

    /// 读取单个元素属性（如 AXMainWindow / AXFocusedWindow）。
    pub fn element_attribute(&self, name: &str) -> Option<AxElement> {
        let value = self.copy_attribute_value(name)?;
        // SAFETY：Copy API 返回 caller-owned 引用，直接接管。
        Self::from_retained(value as AXUIElementRef)
    }

    /// 属性是否可写（窗口 AXPosition/AXSize 不可写时表示不可移动/缩放）。
    pub fn is_attribute_settable(&self, name: &str) -> bool {
        let raw = self.raw;
        let cf_attr = CFString::new(name);
        ax_catch(|| {
            let mut settable: u8 = 0;
            let code = unsafe {
                AXUIElementIsAttributeSettable(raw, cf_attr.as_concrete_TypeRef(), &mut settable)
            };
            code == 0 && settable != 0
        })
        .unwrap_or(false)
    }

    /// 设置窗口左上角位置（AX 全局坐标，主屏左上原点，points）。
    pub fn set_position(&self, x: f64, y: f64) -> Result<(), AxError> {
        let point = CGPoint { x, y };
        self.set_ax_value(
            "AXPosition",
            AX_VALUE_TYPE_CG_POINT,
            &point as *const CGPoint as *const c_void,
        )
    }

    /// 设置窗口尺寸（points）。
    pub fn set_size(&self, width: f64, height: f64) -> Result<(), AxError> {
        let size = CGSize { width, height };
        self.set_ax_value(
            "AXSize",
            AX_VALUE_TYPE_CG_SIZE,
            &size as *const CGSize as *const c_void,
        )
    }

    fn set_ax_value(&self, name: &str, the_type: u32, value: *const c_void) -> Result<(), AxError> {
        let raw = self.raw;
        let cf_attr = CFString::new(name);
        match ax_catch(|| unsafe {
            let ax_value = AXValueCreate(the_type, value);
            if ax_value.is_null() {
                return AxError::Failure as i32;
            }
            let code = AXUIElementSetAttributeValue(raw, cf_attr.as_concrete_TypeRef(), ax_value);
            CFRelease(ax_value);
            code
        }) {
            Some(code) => {
                let err = AxError::from_raw(code);
                if err.is_success() { Ok(()) } else { Err(err) }
            }
            None => Err(AxError::Failure),
        }
    }

    /// 底层拷贝属性值，返回 caller-owned CFTypeRef（成功时）。
    /// 用 ax_catch 包裹，防止异常控件抛 NSException 导致进程 abort。
    fn copy_attribute_value(&self, name: &str) -> Option<CFTypeRef> {
        let raw = self.raw;
        let cf_name = CFString::new(name);
        ax_catch(|| {
            let mut value: CFTypeRef = std::ptr::null();
            let code = unsafe {
                AXUIElementCopyAttributeValue(raw, cf_name.as_concrete_TypeRef(), &mut value)
            };
            let err = AxError::from_raw(code);
            if !err.is_success() || value.is_null() {
                None
            } else {
                Some(value)
            }
        })
        .flatten()
    }
}

impl Drop for AxElement {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY：构造时持有引用，Drop 时释放。
            unsafe { CFRelease(self.raw as CFTypeRef) };
        }
    }
}

/// 探测辅助功能授权。
pub fn is_process_trusted() -> bool {
    // SAFETY：无副作用。
    unsafe { AXIsProcessTrusted() != 0 }
}
/// 解析屏幕坐标处顶层元素的进程 pid（desktop_mouse 定向投递用）。
///
/// 坐标为 AX 全局坐标（主屏左上原点，points）。命中返回目标进程；
/// 无 AX 树区域或坐标无效时返回 `None`，调用方走 warp 兜底。
pub fn pid_at_position(x: f64, y: f64) -> Option<i32> {
    // SAFETY：SystemWide 元素为 caller-owned 引用，用完 release。
    let system_wide = unsafe { AXUIElementCreateSystemWide() };
    if system_wide.is_null() {
        return None;
    }
    let mut element: AXUIElementRef = std::ptr::null();
    let code = unsafe { AXUIElementCopyElementAtPosition(system_wide, x, y, &mut element) };
    unsafe { CFRelease(system_wide as CFTypeRef) };
    if code != 0 || element.is_null() {
        return None;
    }
    let mut pid: i32 = 0;
    let ok = unsafe { AXUIElementGetPid(element, &mut pid) } == 0;
    unsafe { CFRelease(element as CFTypeRef) };
    ok.then_some(pid)
}

/// retain 一个 CFTypeRef 并返回新引用。
fn retain(ptr: *const c_void) -> *const c_void {
    if ptr.is_null() {
        return ptr;
    }
    // SAFETY：对非空 CF 对象增引用计数。
    unsafe { CFRetain(ptr as CFTypeRef) }
}

/// 把 CFTypeRef 按真实类型解释为字符串。
/// 仅当值为 CFString 或 CFNumber 时转换；其他类型释放后返回 None，
/// 避免把数字/数组等误当作 CFString 解释导致类型错误。
fn cf_type_as_string(ptr: *const c_void) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY：CFGetTypeID 读取 CF 对象类型 ID，无副作用。
    let type_id = unsafe { CFGetTypeID(ptr) };
    let string_type = unsafe { CFStringGetTypeID() };
    let number_type = unsafe { CFNumberGetTypeID() };
    if type_id == string_type {
        // SAFETY：ptr 已确认是 CFString，wrap_under_create_rule 接管所有权。
        let cf_string = unsafe { CFString::wrap_under_create_rule(ptr as CFStringRef) };
        Some(cf_string.to_string())
    } else if type_id == number_type {
        // SAFETY：ptr 已确认是 CFNumber，尝试读为 f64 再转字符串。
        let value = unsafe { read_cf_number_as_f64(ptr) };
        unsafe { CFRelease(ptr) };
        value.map(|v| v.to_string())
    } else {
        // 未知类型：释放并返回 None。
        unsafe { CFRelease(ptr) };
        None
    }
}

/// 将 CFNumber 读为 f64（优先 float64，回退 int）。
unsafe fn read_cf_number_as_f64(ptr: *const c_void) -> Option<f64> {
    let mut float_val: f64 = 0.0;
    // SAFETY：ptr 已确认为 CFNumber，按 float64 类型读取。
    if unsafe {
        CFNumberGetValue(
            ptr,
            CF_NUMBER_FLOAT64_TYPE,
            &mut float_val as *mut f64 as *mut c_void,
        )
    } != 0
    {
        return Some(float_val);
    }
    let mut int_val: i64 = 0;
    // SAFETY：回退按 int 类型读取。
    if unsafe {
        CFNumberGetValue(
            ptr,
            CF_NUMBER_INT_TYPE,
            &mut int_val as *mut i64 as *mut c_void,
        )
    } != 0
    {
        return Some(int_val as f64);
    }
    None
}

/// 包裹可能抛 Objective-C NSException 的 AX 调用。
///
/// macOS 辅助功能 API 在访问某些异常控件时会抛 NSException（而非返回错误码），
/// Rust 无法捕获 foreign exception，会导致进程 abort。本函数用 objc2 的
/// `exception::catch` 捕获异常，异常时返回 None，保证宿主不崩溃。
pub fn ax_catch<R: Default>(closure: impl FnOnce() -> R) -> Option<R> {
    objc2::exception::catch(std::panic::AssertUnwindSafe(closure)).ok()
}
