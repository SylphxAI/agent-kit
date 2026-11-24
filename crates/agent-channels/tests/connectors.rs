use agent_channels::connector::{signing::sign_webhook, *};
use agent_channels::telegram::BotIdentity;
use async_trait::async_trait;
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use serde_json::{Value, json};
use std::{sync::Mutex, time::Duration};

const NOW: i64 = 1_700_000_000;
const CTX: DeliveryContext<'_> = DeliveryContext {
    delivery_id: "evt_1",
    unix_seconds: NOW,
};
fn secret(s: &str) -> Secret {
    Secret::new(s)
}
fn input<'a>(body: &'a [u8], headers: &'a [(&'a str, &'a str)]) -> WebhookRequest<'a> {
    WebhookRequest {
        method: "POST",
        query: &[],
        headers,
        body,
        unix_seconds: NOW,
    }
}
fn connector(inbound: InboundAuth) -> Connector {
    Connector::new(inbound.kind(), Some(inbound), None).unwrap()
}
fn sender(outbound: OutboundAuth) -> Connector {
    Connector::new(outbound.kind(), None, Some(outbound)).unwrap()
}
fn msg(to: &str, text: &str) -> OutboundMessage {
    OutboundMessage {
        address: Address {
            conversation: to.into(),
            thread: None,
            reply_target: None,
        },
        content: Content {
            text: text.into(),
            ..Default::default()
        },
    }
}
fn wire(req: &DeliveryRequest) -> Value {
    serde_json::from_slice(&req.body).unwrap()
}
fn apply_headers<'a>(headers: &'a [(&'static str, Secret)]) -> Vec<(&'static str, &'a str)> {
    headers.iter().map(|(k, v)| (*k, v.expose())).collect()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write;
        write!(s, "{b:02x}").unwrap();
        s
    })
}
fn mac256(bytes: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    let mut h = Hmac::<sha2::Sha256>::new_from_slice(b"test-key").unwrap();
    h.update(bytes);
    h.finalize().into_bytes().to_vec()
}

#[test]
fn telegram_normalizes_real_topics_caption_mentions_and_ignores_reactions() {
    let c = connector(InboundAuth::Telegram {
        webhook_token: secret("fixture"),
        bot: BotIdentity::new(Some("example_bot"), Some(42)),
    });
    let headers = [("x-telegram-bot-api-secret-token", "fixture")];
    let message = json!({"update_id": 100, "message": {"chat": {"id": -12, "type":"supergroup"},"from":{"id":7},"message_id":35,"message_thread_id":11,"is_topic_message":true,"caption":"🙂 @example_bot", "caption_entities":[{"type":"mention","offset":3,"length":12}],"photo":[{"file_id":"low"},{"file_id":"high"}]}});
    let data = message.to_string();
    let e = &c.ingest(input(data.as_bytes(), &headers)).unwrap().events[0];
    assert_eq!(
        e.address,
        Address {
            conversation: "-12".into(),
            thread: Some("11".into()),
            reply_target: Some("35".into())
        }
    );
    assert_eq!(
        e.content.media[0].source,
        MediaSource::ProviderId("high".into())
    );
    assert_eq!(e.content.mentions[0].id, "example_bot");
    assert!(!e.context_only);
    assert!(
        c.ingest(input(br#"{"update_id":1,"message_reaction":{}}"#, &headers))
            .unwrap()
            .events
            .is_empty()
    );
    assert_eq!(
        c.ingest(input(data.as_bytes(), &[])),
        Err(IngressError::Unauthorized)
    );
    assert_eq!(
        c.ingest(input(b"{", &headers)),
        Err(IngressError::Malformed)
    );
    assert_eq!(
        c.ingest(input(br#"{"update_id":1,"message":{}}"#, &headers)),
        Err(IngressError::Malformed)
    );
    assert_eq!(
        c.with_body_limit(10)
            .ingest(input(data.as_bytes(), &headers)),
        Err(IngressError::TooLarge)
    );
}

#[test]
fn telegram_literal_text_inline_buttons_and_topic_delivery() {
    let c = sender(OutboundAuth::Telegram {
        bot_token: secret("123:test-fixture"),
    });
    let mut m = msg("-12", "🙂 <x>&_*!");
    m.address.thread = Some("11".into());
    m.address.reply_target = Some("35".into());
    m.content.mentions.push(Mention {
        id: "7".into(),
        label: Some("reader".into()),
    });
    m.content.buttons.push(Button {
        label: "Open".into(),
        action: ButtonAction::Callback("confirm".into()),
    });
    let request = c.prepare(&m, CTX).unwrap();
    assert_eq!(request.path, "/bot123:test-fixture/sendMessage");
    assert_eq!(
        wire(&request),
        json!({"chat_id":-12,"message_thread_id":11,"reply_parameters":{"message_id":35,"allow_sending_without_reply":false},"text":"🙂 <x>&_*! @reader","entities":[{"type":"text_mention","offset":11,"length":7,"user":{"id":7}}],"reply_markup":{"inline_keyboard":[[{"text":"Open","callback_data":"confirm"}]]}})
    );
    m.address.thread = Some("0".into());
    assert_eq!(
        c.prepare(&m, CTX).err(),
        Some(DeliveryError::InvalidMessage)
    );
}

#[test]
fn slack_verifies_raw_bytes_and_keeps_thread_route() {
    let body = br#"{"type":"event_callback","team_id":"T1","event_id":"evt","event":{"type":"app_mention","channel":"C1","user":"U1","ts":"1700000000.000004","thread_ts":"1700000000.000001","text":"<@UBOT> &amp; &lt;"}}"#;
    let make_sig = |ts| {
        let mut b = format!("v0:{ts}:").into_bytes();
        b.extend_from_slice(body);
        format!("v0={}", hex(&mac256(&b)))
    };
    let tag = make_sig(NOW);
    let h = [
        ("X-Slack-Request-Timestamp", "1700000000"),
        ("X-Slack-Signature", tag.as_str()),
    ];
    let c = connector(InboundAuth::Slack {
        signing_secret: secret("test-key"),
        team_id: "T1".into(),
        bot_user_id: Some("UBOT".into()),
    });
    let event = &c.ingest(input(body, &h)).unwrap().events[0];
    assert_eq!(event.address.thread.as_deref(), Some("1700000000.000001"));
    assert_eq!(event.content.mentions[0].id, "UBOT");
    assert_eq!(event.content.text, "<@UBOT> & <");
    assert_eq!(c.ingest(input(b"{}", &h)), Err(IngressError::Unauthorized));
    assert_eq!(
        c.ingest(WebhookRequest {
            unix_seconds: NOW + 301,
            ..input(body, &h)
        }),
        Err(IngressError::Unauthorized)
    );
    let mut m = msg("C1", "<!here> & hello");
    m.address = event.address.clone();
    m.content.mentions.push(Mention {
        id: "U2".into(),
        label: None,
    });
    let c = sender(OutboundAuth::Slack {
        bot_token: secret("fixture"),
    });
    let request = c.prepare(&m, CTX).unwrap();
    assert_eq!(request.path, "/chat.postMessage");
    assert_eq!(wire(&request)["text"], "&lt;!here&gt; &amp; hello <@U2>");
    assert_eq!(wire(&request)["thread_ts"], "1700000000.000001");
    assert_eq!(wire(&request)["unfurl_links"], false);
}

#[test]
fn discord_signed_interaction_and_opt_in_mentions() {
    let key = ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::new([7; 32]));
    let body = br#"{"type":3,"application_id":"10","id":"11","channel_id":"12","member":{"user":{"id":"13"}},"message":{"id":"14"},"data":{"custom_id":"ack"}}"#;
    let tag = hex(key
        .sk
        .sign([b"1700000000".as_slice(), body].concat(), None)
        .as_ref());
    let h = [
        ("X-Signature-Ed25519", tag.as_str()),
        ("X-Signature-Timestamp", "1700000000"),
    ];
    let c = connector(InboundAuth::Discord {
        public_key: *key.pk,
        application_id: "10".into(),
    });
    let outcome = c.ingest(input(body, &h)).unwrap();
    assert_eq!(outcome.events[0].action_id.as_deref(), Some("ack"));
    assert_eq!(outcome.response, WebhookResponse::Json(json!({"type":6})));
    assert_eq!(c.ingest(input(b"{}", &h)), Err(IngressError::Unauthorized));
    let mut m = msg("12", "@everyone <@99>");
    m.address.thread = Some("16".into());
    m.address.reply_target = Some("14".into());
    m.content.mentions.push(Mention {
        id: "13".into(),
        label: None,
    });
    let c = sender(OutboundAuth::Discord {
        bot_token: secret("fixture"),
    });
    let request = c.prepare(&m, CTX).unwrap();
    assert_eq!(request.path, "/channels/16/messages");
    assert_eq!(
        wire(&request)["allowed_mentions"],
        json!({"parse":[],"replied_user":false,"users":["13"]})
    );
    assert_eq!(wire(&request)["message_reference"]["message_id"], "14");
    m.content.media.push(Media {
        kind: MediaKind::Image,
        source: MediaSource::Url("https://example.test/image".into()),
        name: None,
        content_type: None,
    });
    assert_eq!(
        c.prepare(&m, CTX).err(),
        Some(DeliveryError::UnsupportedContent)
    );
}

#[test]
fn standard_webhook_verifies_signed_id_time_body_and_email_refuses_header_injection() {
    let payload = br#"{"sender_id":"external","address":{"conversation":"room","thread":"topic","reply_target":"msg"},"content":{"text":"hello"}}"#;
    let key = secret("whsec_dGVzdC1rZXk=");
    let signed = sign_webhook(&key, "event_7", NOW, payload).unwrap();
    let h = apply_headers(&signed);
    for auth in [
        InboundAuth::Webhook {
            signing_key: key.clone(),
        },
        InboundAuth::Custom {
            signing_key: key.clone(),
        },
        InboundAuth::Email {
            signing_key: key.clone(),
        },
    ] {
        let c = connector(auth);
        let e = &c.ingest(input(payload, &h)).unwrap().events[0];
        assert_eq!(e.channel, c.kind());
        assert_eq!(e.event_id, "event_7");
        assert_eq!(e.address.thread.as_deref(), Some("topic"));
        assert_eq!(
            serde_json::from_value::<InboundEvent>(serde_json::to_value(e).unwrap()).unwrap(),
            *e
        );
        for changed in [b"{}".as_slice(), b"null".as_slice()] {
            assert_eq!(
                c.ingest(input(changed, &h)),
                Err(IngressError::Unauthorized)
            );
        }
        let mut repeated = h.clone();
        repeated.push(("Webhook-Id", "event_7"));
        assert_eq!(
            c.ingest(input(payload, &repeated)),
            Err(IngressError::Unauthorized)
        );
        assert_eq!(
            c.ingest(WebhookRequest {
                unix_seconds: NOW + 301,
                ..input(payload, &h)
            }),
            Err(IngressError::Unauthorized)
        );
        let broken = sign_webhook(&key, "event_8", NOW, b"{}").unwrap();
        assert_eq!(
            c.ingest(input(b"{}", &apply_headers(&broken))),
            Err(IngressError::Malformed)
        );
    }
    assert_eq!(
        sign_webhook(&secret("whsec_!"), "evt", NOW, b"{}").err(),
        Some(DeliveryError::InvalidConfiguration)
    );
    let mailbox = |user: &str| format!("{user}@{}", "example.test");
    let c = sender(OutboundAuth::Email {
        from: mailbox("send"),
        signing_key: key.clone(),
    });
    let mut m = msg(&mailbox("receive"), "plain <text>");
    m.address.reply_target = Some("msg-1".into());
    let request = c.prepare(&m, CTX).unwrap();
    assert_eq!(wire(&request)["in_reply_to"], "msg-1");
    assert_eq!(wire(&request)["text"], "plain <text>");
    assert_eq!(
        signing::verify_webhook(&key, input(&request.body, &apply_headers(&request.headers)))
            .unwrap(),
        "evt_1"
    );
    m.address.conversation.push_str("\r\nBcc: another");
    assert_eq!(
        c.prepare(&m, CTX).err(),
        Some(DeliveryError::InvalidMessage)
    );
    let c = sender(OutboundAuth::Webhook {
        signing_key: key.clone(),
    });
    let m = msg("route", "outbound");
    let request = c.prepare(&m, CTX).unwrap();
    assert_eq!(wire(&request), serde_json::to_value(m).unwrap());
    assert!(
        signing::verify_webhook(&key, input(&request.body, &apply_headers(&request.headers)))
            .is_ok()
    );
}

#[test]
fn feishu_signed_encrypted_message_and_reply_format() {
    use aes::cipher::{BlockEncryptMut as _, KeyIvInit as _, block_padding::Pkcs7};
    use sha2::{Digest as _, Sha256};
    let event = json!({"schema":"2.0","header":{"token":"verify-fixture","app_id":"app1","event_id":"evt1","event_type":"im.message.receive_v1"},"event":{"sender":{"sender_type":"user","sender_id":{"open_id":"ou1"}},"message":{"chat_id":"oc1","message_id":"om2","thread_id":"omt1","root_id":"om1","message_type":"text","content":"{\"text\":\"hello\"}"}}});
    let iv = [5; 16];
    let enc = cbc::Encryptor::<aes::Aes256>::new(&Sha256::digest(b"test-key"), &iv.into())
        .encrypt_padded_vec_mut::<Pkcs7>(event.to_string().as_bytes());
    let body = json!({"encrypt":STANDARD.encode([iv.as_slice(), &enc].concat())}).to_string();
    let tag = format!(
        "{:x}",
        Sha256::digest([b"1700000000nonce-test-key".as_slice(), body.as_bytes()].concat())
    );
    let h = [
        ("X-Lark-Signature", tag.as_str()),
        ("X-Lark-Request-Timestamp", "1700000000"),
        ("X-Lark-Request-Nonce", "nonce-"),
    ];
    let c = connector(InboundAuth::Feishu {
        encrypt_key: secret("test-key"),
        verification_token: secret("verify-fixture"),
        app_id: "app1".into(),
    });
    let e = &c.ingest(input(body.as_bytes(), &h)).unwrap().events[0];
    assert_eq!(e.address.thread.as_deref(), Some("om1"));
    assert_eq!(
        c.ingest(input(body.as_bytes(), &[])),
        Err(IngressError::Unauthorized)
    );
    let mut m = msg("oc1", "<at user_id=\"all\">&");
    m.address = e.address.clone();
    let c = sender(OutboundAuth::Feishu {
        access_token: secret("fixture"),
    });
    let req = c.prepare(&m, CTX).unwrap();
    assert_eq!(req.path, "/open-apis/im/v1/messages/om1/reply");
    let b = wire(&req);
    assert_eq!(b["reply_in_thread"], true);
    assert_eq!(
        serde_json::from_str::<Value>(b["content"].as_str().unwrap()).unwrap()["text"],
        "&lt;at user_id=&quot;all&quot;&gt;&amp;"
    );
}

#[test]
fn dingtalk_rejects_stale_or_wrong_robot_and_preserves_group_target() {
    let tag = STANDARD.encode(mac256(b"1700000000000\ntest-key"));
    let h = [("timestamp", "1700000000000"), ("sign", tag.as_str())];
    let body = br#"{"robotCode":"robot1","senderStaffId":"staff1","conversationType":"2","conversationId":"cidAB+=","msgtype":"text","msgId":"id1","text":{"content":"hello"}}"#;
    let c = connector(InboundAuth::Dingtalk {
        app_secret: secret("test-key"),
        robot_code: "robot1".into(),
    });
    let e = &c.ingest(input(body, &h)).unwrap().events[0];
    assert_eq!(e.address.conversation, "group:cidAB+=");
    assert_eq!(
        c.ingest(WebhookRequest {
            unix_seconds: NOW + 3601,
            ..input(body, &h)
        }),
        Err(IngressError::Unauthorized)
    );
    let bad = std::str::from_utf8(body)
        .unwrap()
        .replace("robot1", "robot2");
    assert_eq!(
        c.ingest(input(bad.as_bytes(), &h)),
        Err(IngressError::Unauthorized)
    );
    let c = sender(OutboundAuth::Dingtalk {
        access_token: secret("fixture"),
        robot_code: "robot1".into(),
        group: true,
    });
    let req = c
        .prepare(&msg(&e.address.conversation, "**literal**"), CTX)
        .unwrap();
    assert_eq!(req.path, "/v1.0/robot/groupMessages/send");
    let b = wire(&req);
    assert_eq!(b["openConversationId"], "cidAB+=");
    assert_eq!(b["msgKey"], "sampleText");
    assert_eq!(b["msgParam"], r#"{"content":"**literal**"}"#);
}

fn wx_seal(data: &[u8], receiver: &str) -> (String, String) {
    use aes::cipher::{BlockEncryptMut as _, KeyIvInit as _, block_padding::NoPadding};
    let mut plain = vec![4u8; 16];
    plain.extend_from_slice(&(data.len() as u32).to_be_bytes());
    plain.extend_from_slice(data);
    plain.extend_from_slice(receiver.as_bytes());
    let padding = 32 - plain.len() % 32;
    plain.extend(std::iter::repeat_n(padding as u8, padding));
    let key = [3u8; 32];
    (
        STANDARD.encode(
            cbc::Encryptor::<aes::Aes256>::new_from_slices(&key, &key[..16])
                .unwrap()
                .encrypt_padded_vec_mut::<NoPadding>(&plain),
        ),
        STANDARD_NO_PAD.encode(key),
    )
}
fn wx_sig(parts: &[&str]) -> String {
    use sha1::{Digest as _, Sha1};
    let mut parts = parts.to_vec();
    parts.sort_unstable();
    format!("{:x}", Sha1::digest(parts.concat()))
}

#[test]
fn wecom_safe_mode_binds_recipient_and_agent_and_formats_media() {
    let xml = br#"<xml><ToUserName><![CDATA[corp1]]></ToUserName><FromUserName>user1</FromUserName><AgentID>123</AgentID><MsgId>7</MsgId><MsgType>text</MsgType><Content><![CDATA[hello]]></Content></xml>"#;
    let (enc, key) = wx_seal(xml, "corp1");
    let sig = wx_sig(&["test-key", "1700000000", "nonce", &enc]);
    let query = [
        ("timestamp", "1700000000"),
        ("nonce", "nonce"),
        ("msg_signature", sig.as_str()),
    ];
    let body = format!("<xml><Encrypt><![CDATA[{enc}]]></Encrypt></xml>");
    let c = |agent_id, corp: &str| {
        connector(InboundAuth::Wecom {
            token: secret("test-key"),
            encoding_aes_key: secret(&key),
            receiver_id: corp.into(),
            agent_id,
        })
    };
    let req = WebhookRequest {
        query: &query,
        ..input(body.as_bytes(), &[])
    };
    let e = &c(123, "corp1").ingest(req).unwrap().events[0];
    assert_eq!(e.content.text, "hello");
    assert_eq!(e.address.conversation, "user1");
    assert_eq!(c(124, "corp1").ingest(req), Err(IngressError::Unauthorized));
    assert_eq!(c(123, "corp2").ingest(req), Err(IngressError::Unauthorized));
    assert_eq!(
        c(123, "corp1").ingest(WebhookRequest { body: xml, ..req }),
        Err(IngressError::Unauthorized)
    );
    let c = sender(OutboundAuth::Wecom {
        access_token: secret("fixture"),
        agent_id: 123,
    });
    let mut m = msg("user1", "");
    m.content.media.push(Media {
        kind: MediaKind::Image,
        source: MediaSource::ProviderId("media1".into()),
        name: None,
        content_type: None,
    });
    let req = c.prepare(&m, CTX).unwrap();
    assert_eq!(req.path, "/cgi-bin/message/send");
    assert_eq!(
        wire(&req),
        json!({"agentid":123,"touser":"user1","msgtype":"image","image":{"media_id":"media1"}})
    );
}

#[test]
fn wechat_handshake_is_not_authority_for_plaintext_post() {
    let c = connector(InboundAuth::WechatOfficial {
        token: secret("test-key"),
        encoding_aes_key: secret("unused-for-get"),
        app_id: "app1".into(),
    });
    let sig = wx_sig(&["1700000000", "nonce", "test-key"]);
    let query = [
        ("timestamp", "1700000000"),
        ("nonce", "nonce"),
        ("signature", sig.as_str()),
        ("echostr", "challenge"),
    ];
    let req = WebhookRequest {
        query: &query,
        method: "GET",
        ..input(b"", &[])
    };
    assert_eq!(
        c.ingest(req).unwrap().response,
        WebhookResponse::Text("challenge".into())
    );
    assert_eq!(
        c.ingest(WebhookRequest {
            method: "POST",
            body: b"<xml></xml>",
            ..req
        }),
        Err(IngressError::Unauthorized)
    );
    let c = sender(OutboundAuth::WechatOfficial {
        access_token: secret("fixture"),
    });
    let r = c.prepare(&msg("openid", "hello"), CTX).unwrap();
    assert_eq!(r.path, "/cgi-bin/message/custom/send");
    assert_eq!(
        wire(&r),
        json!({"touser":"openid","msgtype":"text","text":{"content":"hello"}})
    );
}

#[test]
fn sms_and_messenger_validate_ingress_and_format_egress() {
    use hmac::{Hmac, Mac};
    let url = "https://example.test/sms";
    let from = "+12025550101";
    let to = "+12025550102";
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(b"test-key").unwrap();
    mac.update(
        format!("{url}Bodyhello & goodbyeFrom{from}MessageSidSM1NumMedia0To{to}").as_bytes(),
    );
    let sig = STANDARD.encode(mac.finalize().into_bytes());
    let http = [("X-Twilio-Signature", sig.as_str())];
    let body =
        b"NumMedia=0&From=%2B12025550101&To=%2B12025550102&Body=hello+%26+goodbye&MessageSid=SM1";
    let c = connector(InboundAuth::Sms {
        auth_token: secret("test-key"),
        callback_url: url.into(),
        destination: to.into(),
    });
    assert_eq!(
        c.ingest(input(body, &http)).unwrap().events[0].content.text,
        "hello & goodbye"
    );
    assert_eq!(
        c.ingest(input(b"Body=other", &http)),
        Err(IngressError::Unauthorized)
    );
    let c = sender(OutboundAuth::Sms {
        account_sid: "AC1".into(),
        auth_token: secret("fixture"),
        from: to.into(),
    });
    let req = c.prepare(&msg(from, "hello & goodbye"), CTX).unwrap();
    assert_eq!(req.path, "/2010-04-01/Accounts/AC1/Messages.json");
    assert_eq!(
        std::str::from_utf8(&req.body).unwrap(),
        "To=%2B12025550101&From=%2B12025550102&Body=hello+%26+goodbye"
    );
    let data = br#"{"object":"page","entry":[{"id":"123","messaging":[{"message":{"is_echo":true}},{"sender":{"id":"44"},"recipient":{"id":"123"},"message":{"mid":"message1","text":"hello"}}]}]}"#;
    let tag = format!("sha256={}", hex(&mac256(data)));
    let h = [("X-Hub-Signature-256", tag.as_str())];
    let c = connector(InboundAuth::Messenger {
        app_secret: secret("test-key"),
        verify_token: secret("verify"),
        page_id: "123".into(),
    });
    assert_eq!(c.ingest(input(data, &h)).unwrap().events.len(), 1);
    assert_eq!(c.ingest(input(data, &[])), Err(IngressError::Unauthorized));
    let c = sender(OutboundAuth::Messenger {
        page_access_token: secret("fixture"),
        page_id: "123".into(),
    });
    let r = c.prepare(&msg("44", "hello"), CTX).unwrap();
    assert_eq!(r.path, "/123/messages");
    assert_eq!(
        wire(&r),
        json!({"recipient":{"id":"44"},"messaging_type":"RESPONSE","message":{"text":"hello"}})
    );
}

#[test]
fn capability_disclosures_and_delivery_dispositions() {
    use DeliveryError::*;
    assert!(
        Connector::new(
            ChannelKind::Email,
            None,
            Some(OutboundAuth::Telegram {
                bot_token: secret("123:abc")
            })
        )
        .is_err()
    );
    let t = sender(OutboundAuth::Telegram {
        bot_token: secret("123:abc"),
    });
    let s = sender(OutboundAuth::Slack {
        bot_token: secret("fixture"),
    });
    let w = sender(OutboundAuth::Wecom {
        access_token: secret("fixture"),
        agent_id: 7,
    });
    assert!(t.capabilities().threads && t.capabilities().attachments && t.capabilities().buttons);
    assert!(s.capabilities().cards && !s.capabilities().media);
    assert!(!w.capabilities().threads);
    let check = |c: &Connector, status, value: Value| {
        c.check_response(&DeliveryResponse {
            status,
            retry_after: None,
            body: value.to_string().into_bytes(),
        })
    };
    assert_eq!(
        check(&t, 200, json!({"ok":true,"result":{"message_id":1}})),
        Ok(())
    );
    assert_eq!(
        check(
            &t,
            200,
            json!({"ok":false,"error_code":429,"parameters":{"retry_after":9}})
        ),
        Err(Retryable {
            retry_after: Some(Duration::from_secs(9))
        })
    );
    assert_eq!(
        check(&t, 429, json!(null)),
        Err(Retryable { retry_after: None })
    );
    assert_eq!(check(&t, 403, json!(null)), Err(Permanent));
    assert_eq!(check(&t, 503, json!(null)), Err(UnknownOutcome));
    assert_eq!(
        check(&s, 200, json!({"ok":false,"error":"channel_not_found"})),
        Err(Permanent)
    );
    assert_eq!(check(&s, 200, json!({"ok":true})), Err(UnknownOutcome));
    assert_eq!(
        check(&w, 200, json!({"errcode":0,"invaliduser":"target"})),
        Err(Permanent)
    );
    assert_eq!(
        check(&w, 200, json!({"errcode":-1})),
        Err(Retryable { retry_after: None })
    );
}

#[tokio::test]
async fn only_explicit_delivery_emits_one_send_and_never_a_reaction() {
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);
    #[async_trait]
    impl ChannelTransport for Recorder {
        async fn post(&self, r: DeliveryRequest) -> Result<DeliveryResponse, DeliveryError> {
            self.0.lock().unwrap().push(r.path);
            Ok(DeliveryResponse {
                status: 429,
                retry_after: Some(Duration::from_secs(5)),
                body: vec![],
            })
        }
    }
    let c = Connector::new(
        ChannelKind::Telegram,
        Some(InboundAuth::Telegram {
            webhook_token: secret("fixture"),
            bot: BotIdentity::default(),
        }),
        Some(OutboundAuth::Telegram {
            bot_token: secret("123:key"),
        }),
    )
    .unwrap();
    let t = Recorder::default();
    let h = [("X-Telegram-Bot-Api-Secret-Token", "fixture")];
    let req = input(br#"{"update_id":1,"message":{"message_id":7,"chat":{"id":5,"type":"private"},"from":{"id":5},"text":"hello"}}"#,&h);
    let e = c.ingest(req).unwrap().events.remove(0);
    let m = OutboundMessage {
        address: e.address,
        content: Content {
            text: "reply".into(),
            ..Default::default()
        },
    };
    c.prepare(&m, CTX).unwrap();
    assert_eq!(
        c.deliver(&t, &msg("", "bad"), CTX).await,
        Err(DeliveryError::InvalidMessage)
    );
    assert!(t.0.lock().unwrap().is_empty());
    assert!(c.deliver(&t, &m, CTX).await.unwrap_err().is_retryable());
    assert_eq!(*t.0.lock().unwrap(), ["/bot123:key/sendMessage"]);
}

#[test]
fn cache_scopes_rotation_and_expires_before_provider_deadline() {
    let cache = AccessTokenCache::default();
    let a = TokenKey::new(ChannelKind::Feishu, &["ab", "c"]);
    let b = TokenKey::new(ChannelKind::Feishu, &["a", "bc"]);
    let wrong_provider = TokenKey::new(ChannelKind::Dingtalk, &["ab", "c"]);
    cache.put(a, secret("old"), Duration::from_secs(3600));
    cache.put(a, secret("new"), Duration::from_secs(3600));
    cache.invalidate(&a, &secret("old"));
    assert_eq!(cache.get(&a).unwrap().expose(), "new");
    assert!(cache.get(&b).is_none() && cache.get(&wrong_provider).is_none());
    cache.invalidate(&a, &secret("new"));
    assert!(cache.get(&a).is_none());
    cache.put(a, secret("expiring"), Duration::from_secs(1));
    assert!(cache.get(&a).is_none());
    assert!(
        !format!("{:?}", secret("fixture-sensitive-value")).contains("fixture-sensitive-value")
    );
}

#[test]
fn slack_actions_and_discord_gateway_dispatches_normalize_without_effects() {
    let payload = json!({"type":"block_actions","team":{"id":"T1"},"trigger_id":"123.456.fixture","user":{"id":"U1"},"channel":{"id":"C1"},"container":{"message_ts":"1700000000.000002"},"message":{"thread_ts":"1700000000.000001"},"actions":[{"value":"confirm"}]});
    let mut form = form_urlencoded::Serializer::new(String::new());
    form.append_pair("payload", &payload.to_string());
    let body = form.finish();
    let tag = format!("v0={}", hex(&mac256(format!("v0:{NOW}:{body}").as_bytes())));
    let h = [
        ("X-Slack-Request-Timestamp", "1700000000"),
        ("X-Slack-Signature", tag.as_str()),
        ("content-type", "application/x-www-form-urlencoded"),
    ];
    let c = connector(InboundAuth::Slack {
        signing_secret: secret("test-key"),
        team_id: "T1".into(),
        bot_user_id: None,
    });
    let out = c.ingest(input(body.as_bytes(), &h)).unwrap();
    assert_eq!(out.response, WebhookResponse::Empty);
    assert_eq!(out.events[0].action_id.as_deref(), Some("confirm"));
    assert_eq!(
        out.events[0].address.thread.as_deref(),
        Some("1700000000.000001")
    );
    let frame = br#"{"op":0,"t":"MESSAGE_CREATE","d":{"id":"101","channel_id":"202","author":{"id":"303"},"content":"hello","mentions":[{"id":"404"}]}}"#;
    let e = normalize_discord_gateway_message(frame).unwrap().unwrap();
    assert_eq!(e.address.conversation, "202");
    assert_eq!(e.address.reply_target.as_deref(), Some("101"));
    assert_eq!(e.content.mentions[0].id, "404");
    assert_eq!(
        normalize_discord_gateway_message(br#"{"op":0,"t":"MESSAGE_REACTION_ADD","d":{}}"#),
        Ok(None)
    );
}
