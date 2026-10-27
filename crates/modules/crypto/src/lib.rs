//! The `crypto` module (specification §4.8).
//!
//! Pure synchronous transformations over text-sized inputs: hashing
//! (`sha256`, `sha512`, `md5`), HMAC, base64, random bytes and UUIDs. No
//! signals are emitted.
//!
//! Binary conventions (specification §3.5): every byte-carrying input
//! (`data`, `key`, `encoded`) is either a UTF-8 string — its UTF-8 bytes —
//! or the convention object `{ data = <base64>, encoding = "base64" }`,
//! which decodes raw bytes. Outputs: `hash`/`hmac`/`random_bytes` are hex
//! strings; `base64_decode` returns a UTF-8 string when the bytes are
//! valid UTF-8 and otherwise the convention object with the bytes hex-
//! encoded (`{ data = <hex>, encoding = "hex" }`) — never a silent
//! truncation.

use base64::Engine as _;
use hmac::{Hmac, Mac};
use noisy_core::error::ModuleError;
use noisy_core::module::{
    Command, MethodCall, ModuleContext, ModuleDescriptor, NoisyModule,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256, Sha512};
use tokio::task::JoinHandle;

type HmacSha256 = Hmac<Sha256>;
type HmacSha512 = Hmac<Sha512>;

/// The `crypto` module: a stateless method actor.
pub struct CryptoModule;

impl Default for CryptoModule {
    fn default() -> Self {
        Self::new()
    }
}

impl CryptoModule {
    pub fn new() -> Self {
        Self
    }
}

impl NoisyModule for CryptoModule {
    fn descriptor(&self) -> ModuleDescriptor {
        ModuleDescriptor::new("crypto")
            .version("0.1.0")
            .method_params("hash", ["algorithm", "data"])
            .method_params("hmac", ["algorithm", "key", "data"])
            .method_params("base64_encode", ["data"])
            .method_params("base64_decode", ["encoded"])
            .method_params("random_bytes", ["n"])
            .method("uuid")
    }

    fn start(self: Box<Self>, ctx: ModuleContext) -> JoinHandle<()> {
        let ModuleContext {
            mut commands,
            host: _,
            shutdown,
            config: _,
        } = ctx;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    cmd = commands.recv() => match cmd {
                        Some(Command::MethodCall(call)) => {
                            let reply = serve(&call);
                            call.reply(reply);
                        }
                        // The crypto module issues no async calls.
                        Some(_) => {}
                        None => break,
                    },
                }
            }
            // Drain phase: precise shutdown failures for late callers.
            while let Some(cmd) = commands.recv().await {
                if let Command::MethodCall(call) = cmd {
                    call.reply(Err(ModuleError::ShuttingDown));
                }
            }
        })
    }
}

/// Serves one method call (instant transformations; sync on purpose).
fn serve(call: &MethodCall) -> Result<Value, ModuleError> {
    let params = call.params();
    match call.method() {
        "hash" => {
            let digest = match params["algorithm"].as_str() {
                Some("sha256") => hex::encode(Sha256::digest(bytes(params.get("data"), call)?)),
                Some("sha512") => hex::encode(Sha512::digest(bytes(params.get("data"), call)?)),
                Some("md5") => hex::encode(md5(bytes(params.get("data"), call)?)),
                Some(other) => {
                    return Err(unknown_algorithm(call, other, &["sha256", "sha512", "md5"]))
                }
                None => return Err(need_string(call, "`algorithm`")),
            };
            Ok(json!(digest))
        }
        "hmac" => {
            let key = bytes(params.get("key"), call)?;
            let data = bytes(params.get("data"), call)?;
            let tag = match params["algorithm"].as_str() {
                Some("sha256") => {
                    let mut mac = HmacSha256::new_from_slice(&key)
                        .map_err(|err| bad(call, &format!("hmac: invalid key: {err}")))?;
                    mac.update(&data);
                    hex::encode(mac.finalize().into_bytes())
                }
                Some("sha512") => {
                    let mut mac = HmacSha512::new_from_slice(&key)
                        .map_err(|err| bad(call, &format!("hmac: invalid key: {err}")))?;
                    mac.update(&data);
                    hex::encode(mac.finalize().into_bytes())
                }
                Some("md5") => {
                    return Err(unknown_algorithm(call, "md5", &["sha256", "sha512"]))
                }
                Some(other) => {
                    return Err(unknown_algorithm(call, other, &["sha256", "sha512"]))
                }
                None => return Err(need_string(call, "`algorithm`")),
            };
            Ok(json!(tag))
        }
        "base64_encode" => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes(
                params.get("data"),
                call,
            )?);
            Ok(json!(encoded))
        }
        "base64_decode" => {
            let encoded = params["encoded"].as_str().ok_or_else(|| {
                need_string(call, "`encoded`")
            })?;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|err| bad(call, &format!("base64_decode: {err}")))?;
            match String::from_utf8(decoded) {
                Ok(text) => Ok(json!(text)),
                Err(err) => Ok(json!({
                    "data": hex::encode(err.into_bytes()),
                    "encoding": "hex",
                })),
            }
        }
        "random_bytes" => {
            let n = params["n"].as_u64().ok_or_else(|| {
                bad(call, "random_bytes: `n` must be a non-negative number")
            })?;
            if n > 1024 * 1024 {
                return Err(bad(call, "random_bytes: `n` above 1 MiB is refused"));
            }
            let mut data = vec![0u8; n as usize];
            use rand::RngCore;
            rand::rng().fill_bytes(&mut data);
            Ok(json!(hex::encode(data)))
        }
        "uuid" => Ok(json!(uuid::Uuid::new_v4().to_string())),
        other => Err(ModuleError::unknown_method("crypto", other)),
    }
}

// ───────────────────── plumbing

/// Byte-carrying input: a UTF-8 string or the §3.5 base64 convention.
fn bytes(value: Option<&Value>, call: &MethodCall) -> Result<Vec<u8>, ModuleError> {
    match value {
        Some(Value::String(text)) => Ok(text.as_bytes().to_vec()),
        Some(Value::Object(fields))
            if fields.get("encoding").and_then(Value::as_str) == Some("base64") =>
        {
            let data = fields["data"].as_str().ok_or_else(|| {
                bad(call, "`data` must be a base64 string when encoding is given")
            })?;
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|err| bad(call, &format!("`data` is not valid base64: {err}")))
        }
        _ => Err(bad(call, "input must be a string or { data = <base64>, encoding = \"base64\" }")),
    }
}

fn md5(data: Vec<u8>) -> [u8; 16] {
    let mut hasher = md5::Md5::new();
    hasher.update(&data);
    hasher.finalize().into()
}

fn bad(call: &MethodCall, message: &str) -> ModuleError {
    ModuleError::failed("crypto", format!("crypto.{}: {message}", call.method()))
}

fn need_string(call: &MethodCall, what: &str) -> ModuleError {
    bad(call, &format!("{what} must be a string"))
}

fn unknown_algorithm(call: &MethodCall, got: &str, supported: &[&str]) -> ModuleError {
    bad(
        call,
        &format!("unknown algorithm `{got}` (supported: {})", supported.join(", ")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec surface without signals.
    #[test]
    fn descriptor_declares_the_spec_surface() {
        let descriptor = CryptoModule::new().descriptor();
        descriptor.validate().expect("valid descriptor");
        for method in ["hash", "hmac", "base64_encode", "base64_decode", "random_bytes", "uuid"] {
            assert!(descriptor.method_decl(method).is_some(), "{method}");
        }
        assert!(descriptor.signals.is_empty(), "crypto emits no signals");
    }

    fn probe(method: &str, params: Value) -> Result<Value, ModuleError> {
        let call = MethodCall::for_tests(method, params);
        serve(&call)
    }

    /// Known digests.
    #[test]
    fn hash_known_vectors() {
        // NIST vectors for SHA-256/512 and RFC 1321 for MD5.
        let sha256 = probe("hash", json!({ "algorithm": "sha256", "data": "abc" }))
            .expect("sha256");
        assert_eq!(
            sha256.as_str().unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let sha512 = probe("hash", json!({ "algorithm": "sha512", "data": "abc" }))
            .expect("sha512");
        assert_eq!(
            sha512.as_str().unwrap(),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
                .replace([' ', '\n', '\r'], ""),
        );
        let md5 = probe("hash", json!({ "algorithm": "md5", "data": "abc" }))
            .expect("md5");
        assert_eq!(md5.as_str().unwrap(), "900150983cd24fb0d6963f7d28e17f72");
        let error = probe("hash", json!({ "algorithm": "sh1200", "data": "abc" }))
            .expect_err("unknown algorithm is refused");
        assert!(error.to_string().contains("unknown algorithm"));
    }

    /// RFC 4231 test case 2 for HMAC-SHA256.
    #[test]
    fn hmac_known_vector() {
        let tag = probe(
            "hmac",
            json!({ "algorithm": "sha256", "data": "what do ya want for nothing?", "key": "Jefe" }),
        )
        .expect("hmac-sha256");
        assert_eq!(
            tag.as_str().unwrap(),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// Base64 and binary-input round trips (the §3.5 object form).
    #[test]
    fn base64_and_binary_inputs() {
        let encoded = probe("base64_encode", json!({ "data": "hello" }))
            .expect("encode");
        assert_eq!(encoded.as_str().unwrap(), "aGVsbG8=");

        let decoded = probe("base64_decode", json!({ "encoded": "aGVsbG8=" }))
            .expect("decode");
        assert_eq!(decoded.as_str().unwrap(), "hello");

        // Raw bytes via the convention object: data = base64("hello").
        let through_object = probe(
            "hash",
            json!({ "algorithm": "sha256", "data": { "data": "aGVsbG8=", "encoding": "base64" } }),
        )
        .expect("hash of binary-convention input");
        let direct = probe("hash", json!({ "algorithm": "sha256", "data": "hello" }))
            .expect("hash of text");
        assert_eq!(through_object, direct, "object encoding decodes to the same bytes");

        // Invalid base64 is refused, never truncated.
        let error = probe("base64_decode", json!({ "encoded": "!!!!" }))
            .expect_err("invalid base64");
        assert!(error.to_string().contains("base64_decode"));
    }

    /// `random_bytes` shapes: 2 hex chars per requested byte; UUID shape.
    #[test]
    fn random_bytes_and_uuid_shapes() {
        let bytes = probe("random_bytes", json!({ "n": 16 })).expect("random bytes");
        let hex_string = bytes.as_str().expect("hex string").to_owned();
        assert_eq!(hex_string.len(), 32);
        assert!(hex_string.chars().all(|c| c.is_ascii_hexdigit()));

        let invalid = probe("random_bytes", json!({ "n": "many" }))
            .expect_err("non-number n is refused");
        assert!(invalid.to_string().contains("`n`"));

        let id = probe("uuid", json!({})).expect("uuid");
        let id = id.as_str().expect("uuid string");
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
        assert_eq!(id.as_bytes()[14], b'4', "uuid v4 version digit");
    }

    #[test]
    fn unknown_methods_are_refused() {
        let error = probe("encrypt", json!({})).expect_err("unknown method");
        assert!(error.to_string().contains("unknown method"));
    }
}
