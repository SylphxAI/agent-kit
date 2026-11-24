# Channel connectors

Use `agent_channels::connector` to authenticate provider callbacks, normalize events, and send explicitly routed messages. The existing `telegram`, `reply_target`, and `speech_gate` APIs remain available.

1. Configure a `Connector` per connection with its `InboundAuth` and/or `OutboundAuth`. Select credentials from trusted host configuration. Bound the request before reading it, then pass the original bytes and receipt time to `ingest`.
2. Write the returned acknowledgement with its provider content type. Enforce sender and conversation policy, deduplicate by connection and event ID, and honor `context_only` before scheduling a reply. An empty event list does not authorize a reply.
3. Check outbound capabilities, preserve the event's reply route, and call `prepare` or `deliver` explicitly. Implement `ChannelTransport` with configured provider bases, encoded query parameters, TLS, disabled redirects, and bounded request/response time and size. Append the path to the configured base path (do not replace the base path). Keep paths and queries out of logs because some providers carry credentials there. Never fetch attachment links merely because the callback was authenticated.
4. Retry definite transient refusals with bounded backoff and provider wait hints. Reconcile `UnknownOutcome` before resending. Success indicates provider acceptance, not end-user delivery. The host exchanges credentials; `AccessTokenCache` provides expiry-aware caching.

| Channel | Inbound | Outbound |
| --- | --- | --- |
| Telegram | Authenticated messages, channel posts and button callbacks; real forum topics preserved | Literal text, a media item or file with optional caption, inline buttons and replies |
| Slack | Signed Events API and block button callbacks for the configured team | Text, explicit mentions, thread replies, buttons and cards |
| Discord | Signed interaction webhooks; separate normalizer for messages on the host's authenticated Gateway | Channel/thread messages, opt-in mentions, quotes, embeds and buttons |
| Email | Signed mailbox relay events | A signed JSON request to a mailbox relay, with one sender and recipient |
| SMS | Signed Twilio form for the configured callback URL and destination | Twilio SMS/MMS to E.164 numbers |
| Webhook, Custom | Standard Webhooks v1-signed normalized relay events | Standard Webhooks v1-signed outbound envelopes |
| Feishu | Encrypted callback and URL verification; app-bound message events | Text/mentions, uploaded media references, chat send or message/thread reply |
| DingTalk | Signed HTTP robot text events for the configured robot | Direct or group robot text |
| WeCom | Encrypted callback and handshake; recipient and application checked | Direct text and media IDs |
| WeChat Official | Server handshake and encrypted messages; plaintext POSTs refused | Customer-service text, image and voice by media ID |
| Messenger | Signed page messages and supported postbacks and quick replies | Page replies with text or one attachment URL |

For relay ingress, send JSON with `sender_id`, `address` and `content`, optionally `action_id`. The verified `webhook-id` supplies the event ID. For email, the host's relay handles SMTP/MIME, sender authentication, and loop prevention before signing. DingTalk robot signatures cover the timestamp, not the body: use TLS, keep signature headers private and deduplicate. No connector emits reactions, presence, or progress messages automatically. Unsupported operations return an error rather than silently dropping content or changing routes.
