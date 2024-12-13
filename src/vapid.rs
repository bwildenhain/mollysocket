use std::{
    collections::HashMap,
    fmt::{Display, Formatter},
    ops::Add,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use eyre::{eyre, Result};
use jwt::{header::HeaderType, AlgorithmType, Header, PKeyWithDigest, SignWithKey, Token};
use lazy_static::lazy_static;
use openssl::{
    bn::BigNum,
    ec::{EcGroup, EcKey, EcPoint, PointConversionForm},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
};
use rocket::serde::json;

use crate::config;

lazy_static! {
    static ref KEY: Option<SignerWithPubKey> = get_signer_from_conf().ok();
    /** Cache of VAPID keys */
    static ref VAPID_CACHE: Arc<Mutex<HashMap<String, VapidCache>>> = Arc::new(Mutex::new(HashMap::new()));
}

const DURATION_VAPID: u64 = 4500; /* 1h15 */
const DURATION_VAPID_CACHE: u64 = 3600; /* 1h */

/**
Wrapper containing the signer and the associated public key.
*/
struct SignerWithPubKey {
    signer: PKeyWithDigest<Private>,
    pubkey: String,
}

struct VapidCache {
    header: String,
    expire: Instant,
}

#[derive(Debug)]
pub enum Error {
    VapidKeyError,
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        // We have a single kind of error: VapidKeyError
        write!(f, "VAPID key is probably missing. See https://github.com/mollyim/mollysocket?tab=readme-ov-file#vapid-key")
    }
}

impl std::error::Error for Error {}

pub fn get_vapid_pubkey() -> Result<&'static str> {
    let key = KEY.as_ref().ok_or(Error::VapidKeyError)?;
    Ok(&key.pubkey)
}

/**
Generate VAPID header for origin.
*/
pub fn get_vapid_header(origin: url::Origin) -> Result<String> {
    let key = KEY.as_ref().ok_or(Error::VapidKeyError)?;
    if let Some(h) = get_vapid_header_from_cache(&origin) {
        return Ok(h);
    }
    gen_vapid_header_with_key(origin, key)
}

/**
Get VAPID header from cache if not expire
*/
fn get_vapid_header_from_cache(origin: &url::Origin) -> Option<String> {
    let origin_str = origin.unicode_serialization();
    let now = Instant::now();
    let cache = VAPID_CACHE.lock().unwrap();
    if let Some(c) = cache.get(&origin_str) {
        if c.expire > now {
            log::debug!("Found VAPID from cache");
            Some(c.header.clone())
        } else {
            log::debug!("VAPID from cache has expired");
            None
        }
    } else {
        None
    }
}

fn add_vapid_header_to_cache(origin_str: &str, header: &str) {
    let mut cache = VAPID_CACHE.lock().unwrap();
    cache.insert(
        origin_str.into(),
        VapidCache {
            header: header.into(),
            expire: Instant::now().add(Duration::from_secs(DURATION_VAPID_CACHE)),
        },
    );
}

fn gen_vapid_header_with_key(origin: url::Origin, key: &SignerWithPubKey) -> Result<String> {
    let origin_str = origin.unicode_serialization();
    let header = Header {
        type_: Some(HeaderType::JsonWebToken),
        algorithm: AlgorithmType::Es256,
        ..Default::default()
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        // from_hours is still unstable https://github.com/rust-lang/rust/issues/120301
        .add(Duration::from_secs(DURATION_VAPID))
        .as_secs();
    let claims = json::json!({
        "aud": origin_str,
        "exp": now
    });
    let token = Token::new(header, claims)
        .sign_with_key(&key.signer)
        .unwrap();

    let header = format!("vapid t={},k={}", token.as_str(), &key.pubkey);
    add_vapid_header_to_cache(&origin_str, &header);
    Ok(header)
}

/**
Get [SignerWithPubKey] from the config private key.
*/
fn get_signer_from_conf() -> Result<SignerWithPubKey> {
    match config::get_vapid_privkey() {
        Some(k) => get_signer(k),
        None => Err(eyre!(Error::VapidKeyError)),
    }
}

/**
Get [SignerWithPubKey] from the private key.
*/
fn get_signer(private_bytes: &str) -> Result<SignerWithPubKey> {
    let private_key_bytes = URL_SAFE_NO_PAD.decode(private_bytes).unwrap();
    let private_key_bn = BigNum::from_slice(&private_key_bytes).unwrap();
    let size = private_key_bn.num_bytes();
    if size != 32 {
        if size == 0 {
            log::warn!("No VAPID key was provided.")
        } else {
            log::warn!(
                "The private key has an unexpected size: {}, expected 32.",
                size
            )
        }
        return Err(eyre!(Error::VapidKeyError));
    }
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    let mut ctx = openssl::bn::BigNumContext::new().unwrap();
    let mut public_key_point = EcPoint::new(&group).unwrap();
    public_key_point
        .mul_generator(&group, &private_key_bn, &mut ctx)
        .unwrap();
    let ec_key =
        EcKey::from_private_components(&group, &private_key_bn, &public_key_point).unwrap();
    let public_key_bytes = ec_key
        .public_key()
        .to_bytes(&group, PointConversionForm::UNCOMPRESSED, &mut ctx)
        .unwrap();
    let pubkey = URL_SAFE_NO_PAD.encode(public_key_bytes);

    log::info!("VAPID public key: {:?}", pubkey);
    let key = PKey::from_ec_key(ec_key).unwrap();
    Ok(SignerWithPubKey {
        signer: PKeyWithDigest {
            digest: MessageDigest::sha256(),
            key,
        },
        pubkey,
    })
}

/**
Generate a new VAPID key.
*/
pub fn gen_vapid_key() -> String {
    let key = EcKey::generate(&EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap());
    URL_SAFE_NO_PAD.encode(key.unwrap().private_key().to_vec())
}

#[cfg(test)]
mod tests {

    use super::*;

    const TEST_PRIVKEY: &str = "DSqYuWchrB6yIMYJtidvqANeRQic4uWy34afzZRsZnI";
    const TEST_PUBKEY: &str =
        "BOniQ9xHBPNY9gnQW4o-16vHqOb40pEIMifyUdFsxAgyzVkFMguxw0QrdbZcq8hRjN2zpeInRvKVPlkzABvuTnI";

    /**
    Test [get_signer] returns the right public key.
    */
    #[test]
    fn test_signer_pubkey() {
        assert_eq!(get_signer(TEST_PRIVKEY).unwrap().pubkey, (TEST_PUBKEY))
    }

    /**
    Test [gen_vapid_key] generate a key in the right format.
    */
    #[test]
    fn test_gen_vapid_key() {
        assert_eq!(get_signer(&gen_vapid_key()).unwrap().pubkey.len(), 87);
    }

    /**
    Test vapid with a wrong key
    */
    #[test]
    fn test_wrong_vapid() {
        assert!(get_signer(TEST_PUBKEY).is_err());
        assert!(get_signer("").is_err());
    }

    /**
    To verify the signature with another tool. This must be run with --nocapture:
    `cargo test vapid_other_tool -- -nocapture`
     */
    #[test]
    fn test_vapid_other_tool() {
        let signer = get_signer(&gen_vapid_key()).unwrap();
        let pubkey = signer.signer.key.public_key_to_pem().unwrap();
        let url = url::Url::parse("https://example.tld").unwrap();
        println!("PUB: \n{}", String::from_utf8(pubkey).unwrap());
        println!(
            "header: {}",
            gen_vapid_header_with_key(url.origin(), &signer).unwrap()
        );
    }

    /* The following example depends on the config initialization
        /**
        Test vapid from conf
        */
        #[test]
        fn test_vapid_from_conf() {
            let key = gen_vapid_key();
            env::set_var("MOLLY_VAPID_PRIVKEY", &key);
            config::load_config(None);
            assert_eq!(
                get_signer_from_conf().unwrap().pubkey,
                get_signer(&key).unwrap().pubkey
            )
        }

        /**
        Test unset vapid from conf
        */
        //#[test]
        fn test_no_vapid_from_conf() {
            env::remove_var("MOLLY_VAPID_PRIVKEY");
            config::load_config(None);
            let res = match get_signer_from_conf() {
                Ok(_) => false,
                Err(_) => true,
            };
            assert_eq!(res, true);
        }

    */
}
