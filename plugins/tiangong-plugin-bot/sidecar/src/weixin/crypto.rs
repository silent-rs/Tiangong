//! iLink 媒体加解密——AES-128-ECB + PKCS7 填充。
//!
//! 微信 iLink 协议的图片/文件经 CDN 传输时使用 AES-128-ECB 加密。
//! 密钥随媒体元数据下发（base64 编码），解密后得到原始文件字节。

use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit, block_padding::Pkcs7};
use anyhow::{Context, Result, anyhow};

type Aes128EcbDec = ecb::Decryptor<aes::Aes128>;
type Aes128EcbEnc = ecb::Encryptor<aes::Aes128>;

/// 使用 iLink CDN 所需的 AES-128-ECB + PKCS7 加密文件内容。
pub fn encrypt_media(key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    if key.len() != 16 {
        return Err(anyhow!(
            "iLink 媒体密钥长度应为 16 字节（AES-128），实际 {}",
            key.len()
        ));
    }
    let capacity = plaintext
        .len()
        .checked_add(16)
        .ok_or_else(|| anyhow!("待上传文件过大"))?;
    let mut buf = vec![0; capacity];
    buf[..plaintext.len()].copy_from_slice(plaintext);
    let encryptor =
        Aes128EcbEnc::new_from_slice(key).map_err(|e| anyhow!("构建 AES 加密器失败: {e:?}"))?;
    let ciphertext = encryptor
        .encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
        .map_err(|e| anyhow!("AES-ECB 加密失败: {e:?}"))?;
    Ok(ciphertext.to_vec())
}

/// 用 iLink `media.aes_key` 解密媒体。
///
/// 该字段可能是 16 字节原始密钥的 base64，也可能是 32 位十六进制文本的 base64。
pub fn decrypt_media_base64(key_base64: &str, ciphertext: &[u8]) -> Result<Vec<u8>> {
    let decoded = base64_decode(key_base64)?;
    let key = if decoded.len() == 32 && decoded.iter().all(|byte| byte.is_ascii_hexdigit()) {
        hex::decode(&decoded).context("解析 iLink 媒体十六进制密钥失败")?
    } else {
        decoded
    };
    decrypt_media(&key, ciphertext)
}

/// 用 iLink `image_item.aeskey` 的 32 位十六进制密钥解密媒体。
pub fn decrypt_media_hex(key_hex: &str, ciphertext: &[u8]) -> Result<Vec<u8>> {
    let key = hex::decode(key_hex.trim()).context("解析 iLink 图片十六进制密钥失败")?;
    decrypt_media(&key, ciphertext)
}

fn decrypt_media(key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if key.len() != 16 {
        return Err(anyhow!(
            "iLink 媒体密钥长度应为 16 字节（AES-128），实际 {}",
            key.len()
        ));
    }
    if ciphertext.is_empty() {
        return Err(anyhow!("密文为空，无法解密"));
    }
    if !ciphertext.len().is_multiple_of(16) {
        return Err(anyhow!(
            "AES-128-ECB 密文长度必须是 16 的倍数，实际 {}",
            ciphertext.len()
        ));
    }

    let decryptor =
        Aes128EcbDec::new_from_slice(key).map_err(|e| anyhow!("构建 AES 解密器失败: {e:?}"))?;
    // decrypt_padded_mut 需要可变切片（原地解密去填充）
    let mut buf = ciphertext.to_vec();
    let plaintext = decryptor
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| anyhow!("AES-ECB 解密失败（填充无效）: {e:?}"))?;
    Ok(plaintext.to_vec())
}

/// 解码 base64 字符串为字节（兼容标准与 URL-safe 变体）。
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    let input = input.trim();
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(input))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(input))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(input))
        .context("base64 解码失败")
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn decrypt_roundtrip_known_vector() {
        // AES-128-ECB + PKCS7 测试向量
        // key = 16 字节 0x00..0x0f，明文 "hello weixin"（12 字节）
        let key_bytes: [u8; 16] = (0..16)
            .map(|i| i as u8)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(key_bytes);

        // 用 ecb::Encryptor 加密生成密文
        use aes::cipher::{BlockEncryptMut, block_padding::Pkcs7};
        type Enc = ecb::Encryptor<aes::Aes128>;
        let enc = Enc::new_from_slice(&key_bytes).unwrap();
        let plaintext = b"hello weixin";
        // encrypt_padded_mut 需要可变缓冲区（明文 + 一个块的空间用于填充）
        let mut buf = [0u8; 32]; // 12 字节明文 + 4 字节填充 = 16，给 32 足够余量
        buf[..plaintext.len()].copy_from_slice(plaintext);
        let ciphertext = enc
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
            .expect("加密应成功")
            .to_vec();

        let decrypted = decrypt_media_base64(&key_b64, &ciphertext).expect("解密应成功");
        assert_eq!(decrypted, plaintext);

        let key_hex = hex::encode(key_bytes);
        let decrypted = decrypt_media_hex(&key_hex, &ciphertext).expect("十六进制密钥解密应成功");
        assert_eq!(decrypted, plaintext);

        let encoded_hex = base64::engine::general_purpose::STANDARD.encode(key_hex);
        let decrypted =
            decrypt_media_base64(&encoded_hex, &ciphertext).expect("base64 十六进制密钥解密应成功");
        assert_eq!(decrypted, plaintext);

        let ciphertext = encrypt_media(&key_bytes, plaintext).expect("加密应成功");
        let decrypted =
            decrypt_media_hex(&hex::encode(key_bytes), &ciphertext).expect("新增加密链路应可解密");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn rejects_wrong_key_length() {
        let result = decrypt_media_base64("short", &[0u8; 16]);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_unaligned_ciphertext() {
        // 17 字节密文（非 16 倍数）
        let key_b64 = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        let result = decrypt_media_base64(&key_b64, &[1u8; 17]);
        assert!(result.is_err());
    }
}
