mod bot_api;
mod formatting;

use super::{
    FilePart, bot_api_path, derive_webhook_secret, encode_multipart, verify_webhook_secret,
};
use crate::connector::Secret;

#[test]
fn callback_secret_is_binding_specific_and_checked_exactly() {
    let key = b"key";
    let binding = b"The quick brown fox jumps over the lazy dog";
    let secret = derive_webhook_secret(key, binding);
    assert_eq!(
        secret,
        "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
    );
    assert!(verify_webhook_secret(&secret, Some(&secret)));
    assert!(!verify_webhook_secret(&secret, None));
    assert!(!verify_webhook_secret(&secret, Some(&format!("{secret} "))));
    assert!(!verify_webhook_secret(
        &secret,
        Some(&derive_webhook_secret(key, b"other"))
    ));
}

#[test]
fn request_paths_and_multipart_parts_keep_their_wire_boundaries() {
    let token = Secret::new("123:example");
    assert_eq!(
        bot_api_path(&token, "sendPhoto").unwrap(),
        "/bot123:example/sendPhoto"
    );
    let file = FilePart {
        field: "photo",
        file_name: "example.png",
        content_type: "image/png",
        bytes: b"example bytes",
    };
    let (kind, body) =
        encode_multipart("example-boundary", [("chat_id", "42".into())], file).unwrap();
    assert_eq!(kind, "multipart/form-data; boundary=example-boundary");
    assert_eq!(body, b"--example-boundary\r\nContent-Disposition: form-data; name=\"chat_id\"\r\n\r\n42\r\n--example-boundary\r\nContent-Disposition: form-data; name=\"photo\"; filename=\"example.png\"\r\nContent-Type: image/png\r\n\r\nexample bytes\r\n--example-boundary--\r\n");
}
