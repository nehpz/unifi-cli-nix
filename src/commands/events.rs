use owo_colors::OwoColorize;

use crate::api::UnifiClient;
use crate::output::{OutputConfig, use_color};

/// JSON streaming is one complete message per line; data arrays include both
/// historical-event notifications and live client/device updates.
pub async fn watch(
    session: &crate::api::session::NetworkSession,
    out: OutputConfig,
    site: &str,
    duration: Option<u64>,
    limit: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = session.events(site).await?;
    out.print_message("Streaming Network events and updates. Press Ctrl-C to stop.");
    let mut writer = std::io::stdout();
    let result = watch_to(
        &mut stream,
        out.is_json(),
        duration,
        limit,
        &mut writer,
        async { tokio::signal::ctrl_c().await },
    )
    .await;
    stream.close().await;
    // A consumer such as `head` has deliberately stopped reading.
    match result {
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) =>
        {
            Ok(())
        }
        other => other,
    }
}

pub async fn watch_to(
    stream: &mut crate::api::session::EventStream,
    json: bool,
    duration: Option<u64>,
    limit: Option<u64>,
    writer: &mut dyn std::io::Write,
    stop: impl std::future::Future<Output = std::io::Result<()>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;
    if limit == Some(0) {
        return Ok(());
    }
    let deadline = async {
        if let Some(seconds) = duration {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(deadline, stop);
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(15),
        Duration::from_secs(15),
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut count = 0u64;
    loop {
        tokio::select! {
            biased;
            result = &mut stop => { result?; return Ok(()); },
            _ = &mut deadline => return Ok(()),
            _ = heartbeat.tick() => {
                tokio::select! {
                    biased;
                    result = &mut stop => { result?; return Ok(()); },
                    _ = &mut deadline => return Ok(()),
                    result = stream.heartbeat() => result?,
                }
            }
            message = stream.next() => {
                let mut message = message?;
                redact_secrets(&mut message.data);
                write_update(writer, json, &message.message, &message.data)?;
                writer.flush()?;
                count += 1;
                if limit.is_some_and(|limit| count >= limit) { return Ok(()); }
            }
        }
    }
}

/// Match credential field names at every nesting level, including camelCase
/// spellings. Keep event `key` identifiers and ordinary telemetry intact.
fn redact_secrets(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            for (name, value) in fields {
                let lower = name.to_ascii_lowercase();
                let compact: String = lower
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .collect();
                let sensitive = (compact.ends_with("key") && compact != "key")
                    || compact.ends_with("keys")
                    || matches!(
                        compact.as_str(),
                        "authorization"
                            | "credential"
                            | "credentials"
                            | "syslogkey"
                            | "sshhostkey"
                            | "xsshhostkey"
                            | "xshadow"
                            | "shadow"
                    )
                    || [
                        "password",
                        "passwd",
                        "passphrase",
                        "secret",
                        "secrets",
                        "token",
                        "cookie",
                        "authkey",
                        "vwirekey",
                        "privatekey",
                        "apikey",
                        "psk",
                        "community",
                    ]
                    .iter()
                    .any(|word| compact.contains(word));
                // Presence flags and absent values carry no credential material.
                if sensitive
                    && !value.is_boolean()
                    && !value.is_null()
                    && value.as_str() != Some("")
                {
                    *value = serde_json::Value::String("[REDACTED]".into());
                } else {
                    redact_secrets(value);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_secrets),
        _ => {}
    }
}

fn write_update(
    writer: &mut dyn std::io::Write,
    json: bool,
    message: &str,
    data: &serde_json::Value,
) -> std::io::Result<()> {
    if json {
        writeln!(
            writer,
            "{}",
            serde_json::json!({"message": message, "data": data})
        )
    } else {
        let count = data.as_array().map_or(1, Vec::len);
        let summary = if data.is_null() {
            "no data".into()
        } else if !data.is_array() && !data.is_object() {
            "update received".into()
        } else {
            match message {
                "sta:sync" => format!("{count} client update(s)"),
                "device:sync" | "unifi-device:sync" => format!("{count} device update(s)"),
                "events" => {
                    let items: Vec<_> = match data {
                        serde_json::Value::Array(items) => items.iter().collect(),
                        item => vec![item],
                    };
                    let descriptions: Vec<_> = items
                        .iter()
                        .take(3)
                        .filter_map(|item| item.get("msg").and_then(serde_json::Value::as_str))
                        .map(|msg| truncate(&terminal_safe(msg), 160))
                        .collect();
                    let mut text = format!("{count} event(s)");
                    if !descriptions.is_empty() {
                        text.push_str(": ");
                        text.push_str(&descriptions.join("; "));
                    }
                    if count > 3 {
                        text.push_str("; …");
                    }
                    text
                }
                _ => format!("{count} update(s)"),
            }
        };
        writeln!(
            writer,
            "{}: {summary}",
            truncate(&terminal_safe(message), 80)
        )
    }
}

fn terminal_safe(text: &str) -> String {
    text.chars().map(|c| {
        if c.is_control() || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}') {
            c.escape_unicode().to_string()
        } else { c.to_string() }
    }).collect()
}

pub struct Pagination {
    pub limit: usize,
    pub offset: usize,
    /// Field names already validated against `fields::EVENTS_LIST`.
    pub fields: Option<Vec<String>>,
}

pub async fn list(
    client: &UnifiClient,
    out: OutputConfig,
    pagination: Pagination,
) -> Result<(), Box<dyn std::error::Error>> {
    // Both the legacy query and the alarm fallback limit records before this
    // command skips the offset, so include that prefix in the fetch window.
    let fetch_limit = pagination
        .offset
        .checked_add(pagination.limit)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--offset + --limit is too large",
            )
        })?;
    let events = client.list_events(fetch_limit).await?;
    let total = events.len();
    let paginated: Vec<_> = events
        .into_iter()
        .skip(pagination.offset)
        .take(pagination.limit)
        .collect();

    if out.is_json() {
        let items: Vec<serde_json::Value> = paginated
            .iter()
            .map(|e| {
                let mut obj = serde_json::json!({
                    "key": e.key,
                    "msg": e.msg,
                    "subsystem": e.subsystem,
                    "time": e.time,
                    "datetime": e.datetime,
                });
                if let Some(ref keep) = pagination.fields {
                    let map = obj.as_object_mut().expect("event is a JSON object");
                    map.retain(|k, _| keep.iter().any(|f| f == k));
                }
                obj
            })
            .collect();
        out.print_data(
            &serde_json::to_string_pretty(&serde_json::json!({
                "items": items,
                "total": total,
                "limit": pagination.limit,
                "offset": pagination.offset,
            }))
            .expect("failed to serialize JSON"),
        );
    } else {
        let color = use_color();
        let header = format!(
            "{:<26} {:<10} {:<24} {}",
            "Time", "Subsystem", "Key", "Message"
        );
        if color {
            println!("{}", header.bold());
            println!("{}", "-".repeat(100).dimmed());
        } else {
            println!("{header}");
            println!("{}", "-".repeat(100));
        }

        for e in &paginated {
            let time = e.datetime.as_deref().unwrap_or("-");
            let subsystem = e.subsystem.as_deref().unwrap_or("-");
            let key = e.key.as_deref().unwrap_or("-");
            let msg = truncate(e.msg.as_deref().unwrap_or("-"), 80);

            if color {
                println!(
                    " {:<25} {:<10} {:<24} {}",
                    time.dimmed(),
                    subsystem,
                    key,
                    msg,
                );
            } else {
                println!(" {:<25} {:<10} {:<24} {}", time, subsystem, key, msg);
            }
        }
    }
    out.print_message(&format!("\n{} events", paginated.len()));
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut chars = s.chars();
    let mut result: String = (&mut chars).take(max).collect();
    if chars.next().is_some() {
        result.pop();
        result.push('\u{2026}');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_nested_credentials_but_preserves_event_keys_and_telemetry() {
        let mut value = serde_json::json!({
            "key":"EVT_TEST", "mac":"00:00:00:00:00:01", "tx_bytes":123,
            "data":[{"x_authkey":"fake-auth", "x_vwirekey":"fake-wire",
                "syslog_key":"fake-log", "guest_token":"fake-guest",
                "nested":{"password":"fake-pass", "apiKey":"fake-api",
                    "privateKey":{"value":"fake-private"}, "Authorization":"fake-bearer",
                    "radius_secret":"fake-radius", "wpa_psk":"fake-wifi",
                    "Cookie":"fake-cookie", "csrfToken":"fake-csrf",
                    "snmp_community":"fake-community", "x_ssh_hostkey":"fake-hostkey",
                    "secrets":["fake-secret-array"], "apiKeys":["fake-api-array"],
                    "x_shadow":"fake-hash", "password_hash":"fake-hash-2",
                    "sharedKey":"fake-shared", "token_value":"fake-token-value"}}],
            "has_password":true, "password":null, "passphrase":""
        });
        redact_secrets(&mut value);
        let text = value.to_string();
        assert!(!text.contains("fake-"));
        assert_eq!(value["key"], "EVT_TEST");
        assert_eq!(value["mac"], "00:00:00:00:00:01");
        assert_eq!(value["tx_bytes"], 123);
        assert_eq!(value["has_password"], true);
        assert!(value["password"].is_null());
        assert_eq!(value["passphrase"], "");
        assert_eq!(value["data"][0]["nested"]["privateKey"], "[REDACTED]");
        redact_secrets(&mut value);
        assert_eq!(text, value.to_string());
    }

    #[test]
    fn terminal_summaries_are_bounded_and_do_not_dump_unknown_updates() {
        for (message, expected) in [
            ("sta:sync", "2 client update(s)"),
            ("device:sync", "2 device update(s)"),
            ("unifi-device:sync", "2 device update(s)"),
            ("unknown", "2 update(s)"),
        ] {
            let mut output = Vec::new();
            write_update(
                &mut output,
                false,
                message,
                &serde_json::json!([{"internal":"hidden"}, {}]),
            )
            .unwrap();
            assert_eq!(
                String::from_utf8(output).unwrap(),
                format!("{message}: {expected}\n")
            );
        }
        let mut output = Vec::new();
        write_update(
            &mut output,
            false,
            "events",
            &serde_json::json!([
                {"msg":"a".repeat(1000)}, {"msg":"b".repeat(1000)},
                {"msg":"c".repeat(1000)}, {"msg":"excluded"}
            ]),
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.chars().count() < 520);
        assert!(!text.contains("excluded"));
        assert_eq!(text.lines().count(), 1);
        assert!(text.starts_with("events: 4 event(s): "));
        assert!(text.ends_with("; …\n"));
        for (data, expected) in [
            (serde_json::Value::Null, "no data"),
            (serde_json::json!("opaque"), "update received"),
        ] {
            let mut output = Vec::new();
            write_update(&mut output, false, "events", &data).unwrap();
            assert_eq!(
                String::from_utf8(output).unwrap(),
                format!("events: {expected}\n")
            );
        }
    }

    #[test]
    fn terminal_output_escapes_controls_and_bidi_in_any_field() {
        assert_eq!(
            terminal_safe("hello\u{1b}\u{9b}\u{202e}"),
            "hello\\u{1b}\\u{9b}\\u{202e}"
        );
        for c in ['\u{061c}', '\u{2028}', '\u{2029}', '\u{2060}', '\u{feff}'] {
            assert_eq!(
                terminal_safe(&c.to_string()),
                c.escape_unicode().to_string()
            );
        }
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_one_over() {
        assert_eq!(truncate("hello!", 5), "hell\u{2026}");
    }

    #[test]
    fn truncate_long_string() {
        assert_eq!(
            truncate("hello world, this is long", 10),
            "hello wor\u{2026}"
        );
    }

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn truncate_max_zero() {
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn truncate_max_one() {
        assert_eq!(truncate("hello", 1), "\u{2026}");
    }

    #[test]
    fn truncate_unicode() {
        assert_eq!(
            truncate("\u{03b1}\u{03b2}\u{03b3}\u{03b4}\u{03b5}\u{03b6}", 4),
            "\u{03b1}\u{03b2}\u{03b3}\u{2026}"
        );
    }

    #[test]
    fn truncate_unicode_exact() {
        assert_eq!(
            truncate("\u{03b1}\u{03b2}\u{03b3}\u{03b4}", 4),
            "\u{03b1}\u{03b2}\u{03b3}\u{03b4}"
        );
    }
}
