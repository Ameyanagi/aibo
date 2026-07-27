//! Golden files for the **outgoing** request bodies of a request that carries an
//! image attachment (§2, §5, §10).
//!
//! `request_shape.rs` pins the text bodies. These pin what an attachment adds,
//! and they exist because the three formats disagree about every part of it:
//!
//! | Format | Image content part |
//! |---|---|
//! | Chat Completions | `{"type":"image_url","image_url":{"url":"data:…"}}` |
//! | Responses | `{"type":"input_image","image_url":"data:…"}` |
//! | Anthropic `messages` | `{"type":"image","source":{"type":"base64","media_type":…,"data":…}}` |
//!
//! An `image_url` that is an object in one format and a bare string in the other
//! is a 400 the moment they are confused, and §4 forbids falling back on a 4xx —
//! so it surfaces as a hard error for every user of that provider at once. That
//! is the failure these goldens exist to catch.
//!
//! Re-record with:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p aibo-provider --test attachment_shape
//! ```
//!
//! The image is a fixed 16×16 PNG, inline rather than in a fixture file, so the
//! goldens do not move when the `image` crate's encoder changes. Downscaling has
//! its own tests in `aibo_provider::attachment`; putting a resampled payload in a
//! golden would pin the encoder's output rather than aibo's wire shape.

use std::path::PathBuf;

use aibo_core::types::{
    Attachment, AttachmentSource, Capabilities, ChatRequest, ContentPart, Message, MessageRole,
};
use aibo_provider::anthropic::build_messages_body;
use aibo_provider::attachment::{fold_into_messages, guard};
use aibo_provider::openai_compat::{
    build_chat_completions_body, build_responses_body, cerebras, openai,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::Value;

/// A 16×16 RGBA PNG. Frozen bytes, so a golden diff is always aibo's doing.
const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAYAAAAf8/9hAAAAnklEQVR4Ae3AA6AkWZbG8f937o3IzKdyS2Oubdu2bdu2bdu2bWmMnpZKr54yMyLu+Xa3anqmhztr1a/CZ/s4cBw4DhwHjgPHgePAceA4cBw4DhwHjgPHgeNA5Tj/HlSO8+9B5Tj/HlSO8+9B5Tj/HlSO8+9B5Tj/HlSO8+9B5Tj/HlSO8+9B5Tj/HlSO8+9B5Tj/HlSO8+9B5Tj/HvwjMwgDoBSIctAAAAAASUVORK5CYII=";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The same `request_ask.json` the text goldens use, plus one screen-region
/// image. Sharing the base request keeps the diff between the two golden sets
/// purely "what the attachment added".
fn request() -> ChatRequest {
    let raw =
        std::fs::read_to_string(fixtures().join("request_ask.json")).expect("request fixture");
    let mut req: ChatRequest = serde_json::from_str(&raw).expect("request fixture deserialises");
    req.attachments = vec![Attachment::image(
        AttachmentSource::ScreenRegion,
        BASE64.decode(PNG_BASE64).expect("fixture png"),
        "image/png",
        16,
        16,
        "Region 14:32",
    )];
    req
}

fn seeing() -> Capabilities {
    Capabilities {
        vision: true,
        ..Capabilities::default()
    }
}

fn golden(name: &str, body: &Value) {
    let path = fixtures().join(format!("{name}.request.json"));
    let actual = serde_json::to_string_pretty(body).expect("serialise");

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, format!("{actual}\n")).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden {name}.request.json ({e})"));
    let expected = expected.replace("\r\n", "\n");
    assert_eq!(
        actual.trim(),
        expected.trim(),
        "{name}: request body drifted"
    );
}

/// Chat Completions wants `image_url` as an **object** with a `url` field.
#[test]
fn chat_completions_sends_an_image_url_object() {
    let body = build_chat_completions_body(&request(), &cerebras::quirks());
    let content = &body["messages"][1]["content"];
    let image = content
        .as_array()
        .expect("a message with an image sends array content, not a string")
        .iter()
        .find(|p| p["type"] == "image_url")
        .expect("no image part");
    assert!(
        image["image_url"]["url"]
            .as_str()
            .expect("image_url.url is a string")
            .starts_with("data:image/png;base64,")
    );
    golden("cerebras_vision", &body);
}

/// Responses wants `input_image` with `image_url` as a **bare string**. Sending
/// the Chat Completions object here is a 400.
#[test]
fn responses_sends_an_input_image_with_a_data_url_string() {
    let body = build_responses_body(&request(), &openai::quirks());
    let content = &body["input"][0]["content"];
    let image = content
        .as_array()
        .expect("array content")
        .iter()
        .find(|p| p["type"] == "input_image")
        .expect("no input_image part");
    let url = image["image_url"]
        .as_str()
        .expect("Responses spells image_url as a string, not an object");
    assert!(url.starts_with("data:image/png;base64,"), "{url}");
    assert!(url.ends_with(PNG_BASE64), "the payload must be the bytes");
    golden("openai_responses_vision", &body);
}

/// Anthropic wants a `source` block: no `data:` URL, media type as its own
/// field, bytes as bare base64.
#[test]
fn anthropic_sends_a_base64_source_block_and_no_data_url() {
    let body = build_messages_body(&request());
    let content = &body["messages"][0]["content"];
    let image = content
        .as_array()
        .expect("array content")
        .iter()
        .find(|p| p["type"] == "image")
        .expect("no image block");
    assert_eq!(image["source"]["type"], "base64");
    assert_eq!(image["source"]["media_type"], "image/png");
    assert_eq!(image["source"]["data"], PNG_BASE64);
    assert!(
        !body.to_string().contains("data:image/png"),
        "a `data:` URL in an Anthropic body is the OpenAI shape leaking across"
    );
    golden("anthropic_vision", &body);
}

/// §5 rule 2, on the wire rather than in prose: an image is attacker-controlled
/// input — rendered text defeats every textual filter — so it is fenced and
/// labelled untrusted in **every** format, exactly like a captured selection.
#[test]
fn an_image_is_fenced_as_untrusted_in_every_wire_format() {
    let req = request();
    let bodies = [
        build_chat_completions_body(&req, &cerebras::quirks()),
        build_responses_body(&req, &openai::quirks()),
        build_messages_body(&req),
    ];
    for body in &bodies {
        let rendered = body.to_string();
        assert!(
            rendered.contains("origin=selection"),
            "a dragged screen region has a selection's trust properties: {rendered}"
        );
        assert!(
            rendered.contains("Region 14:32"),
            "the fence must name the attachment: {rendered}"
        );
        assert!(
            rendered.contains("not an instruction"),
            "the fence must say the image is data: {rendered}"
        );
        assert!(
            rendered.contains("authorise a tool call"),
            "§5: context can never authorise a tool call: {rendered}"
        );
    }
}

/// The image goes on the user turn. Anthropic 400s on a `system`-role message,
/// and §5 would have an image in the instructions authorising things.
#[test]
fn the_image_never_lands_in_the_system_prompt() {
    let req = request();

    let anthropic = build_messages_body(&req);
    assert!(anthropic["system"].as_str().is_some());
    assert!(!anthropic["system"].as_str().unwrap().contains("image/png"));

    let responses = build_responses_body(&req, &openai::quirks());
    assert!(
        !responses["instructions"]
            .as_str()
            .unwrap()
            .contains("data:")
    );

    let chat = build_chat_completions_body(&req, &cerebras::quirks());
    assert!(chat["messages"][0]["content"].is_string());
    assert_eq!(chat["messages"][0]["role"], "system");
}

/// The regression the whole feature exists to prevent, asserted at the layer
/// that would commit it: a text-only model must produce an error, never a body
/// with the image quietly removed.
#[test]
fn a_text_only_model_is_refused_rather_than_sent_a_stripped_body() {
    let req = request();
    let err = guard(&Capabilities::default(), &req, Vec::new()).unwrap_err();
    assert_eq!(err.treatment(), aibo_core::error::Treatment::Inline);
    assert!(!err.is_fallback_eligible(), "§4 does not retry this");
    assert!(
        err.to_string().contains("cannot accept image input"),
        "{err}"
    );

    // And the positive control: the same request passes on a model that can see.
    assert!(guard(&seeing(), &req, Vec::new()).is_ok());
}

/// A request with nothing attached must serialise byte-identically to what it
/// did before attachments existed — otherwise every existing golden is a lie and
/// the text path pays for a feature it does not use.
#[test]
fn the_text_path_is_untouched() {
    let mut req = request();
    req.attachments.clear();

    let folded = fold_into_messages(&req);
    assert_eq!(folded.as_ref(), req.messages.as_slice());

    let raw = std::fs::read_to_string(fixtures().join("cerebras.request.json")).expect("golden");
    let expected: Value = serde_json::from_str(&raw).expect("golden parses");
    assert_eq!(
        build_chat_completions_body(&req, &cerebras::quirks()),
        expected
    );
}

/// Attach order is id order (uuid v7) and must survive onto the wire: "the
/// second image" has to mean the same thing to the model as it does in the panel.
#[test]
fn multiple_attachments_keep_their_order_and_each_keeps_its_own_fence() {
    let mut req = request();
    let bytes = BASE64.decode(PNG_BASE64).unwrap();
    req.attachments.push(Attachment::image(
        AttachmentSource::Clipboard,
        bytes,
        "image/png",
        16,
        16,
        "Pasted 14:33",
    ));

    let folded = fold_into_messages(&req);
    let user = folded
        .iter()
        .rfind(|m| m.role == MessageRole::User)
        .expect("user turn");

    // text, then (fence, image) per attachment, in attach order.
    let kinds: Vec<&str> = user
        .parts
        .iter()
        .map(|p| match p {
            ContentPart::Text(_) => "text",
            ContentPart::Untrusted(_) => "fence",
            ContentPart::Image { .. } => "image",
        })
        .collect();
    assert_eq!(
        kinds,
        ["text", "fence", "fence", "image", "fence", "image"],
        "each image must be immediately preceded by its own fence"
    );

    let origins: Vec<String> = user
        .parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Untrusted(b) => Some(format!("{:?}", b.origin)),
            _ => None,
        })
        .collect();
    // The first fence is the request's own captured selection, then one per
    // attachment in attach order — each carrying its own provenance.
    assert_eq!(origins, ["Selection", "Selection", "Clipboard"]);
}

/// Attachments with no user turn to land on get one rather than being dropped.
#[test]
fn attachments_are_never_silently_discarded() {
    let mut req = request();
    req.messages = vec![Message::text(MessageRole::System, "You are aibo.")];
    let body = build_messages_body(&req);
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    assert!(
        messages[0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["type"] == "image")
    );
}
