//! Saving must not leave removed content recoverable from the saved file (Optimised Accounting
//! fork): Save and Save As write a full rewrite of the objects the document still uses, never an
//! incremental update that keeps the earlier revision.
//!
//! Each test plants a unique canary in a synthetic document, removes it with an ordinary edit,
//! saves, and looks for the canary in the saved file directly: in the raw bytes (as literal or
//! hex text), in every indirect object found by scanning the file for `obj` headers (whether or
//! not a cross-reference section lists it), in every object of every revision as the parser reads
//! it (decrypted, streams decoded, object streams unpacked), and in each page's extracted text.

use std::sync::Arc;

use pdfcraft_cos::{Document as Cos, Object, SaveOptions, parse_indirect, serialize};

use super::*;

/// A PDF with a classic cross-reference table: object `i + 1` is `objs[i]`.
fn build(objs: &[String], trailer: &str) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R {trailer} >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

fn stream(body: &str) -> String {
    format!("<< /Length {} >>\nstream\n{body}\nendstream", body.len())
}

fn line(y: u32, text: &str) -> String {
    format!("BT /F1 12 Tf 20 {y} Td ({text}) Tj ET\n")
}

fn open(bytes: Vec<u8>, password: Option<&str>) -> (Session, DocId) {
    let mut s = Session::new().with_clock(|| 1_700_000_000);
    let id = s.open("synthetic.pdf", None, Arc::new(bytes), password).expect("opens");
    (s, id)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// Offsets of every `N G obj` header in `data`.
fn object_headers(data: &[u8]) -> Vec<usize> {
    let digits_before = |mut i: usize| {
        let end = i;
        while i > 0 && data[i - 1].is_ascii_digit() {
            i -= 1;
        }
        (i < end).then_some(i)
    };
    let spaces_before = |mut i: usize| {
        let end = i;
        while i > 0 && matches!(data[i - 1], b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0') {
            i -= 1;
        }
        (i < end).then_some(i)
    };
    let mut out = Vec::new();
    for at in 0..data.len().saturating_sub(2) {
        if &data[at..at + 3] != b"obj" {
            continue;
        }
        let start = spaces_before(at).and_then(digits_before).and_then(spaces_before).and_then(digits_before);
        if let Some(start) = start.filter(|s| *s == 0 || !data[s - 1].is_ascii_alphanumeric()) {
            out.push(start);
        }
    }
    out
}

/// Where `canary` can still be found in `bytes`, the saved file (empty: nowhere).
fn find_canary(bytes: &[u8], canary: &str, password: Option<&str>) -> Vec<String> {
    let needle = canary.as_bytes();
    let hex: Vec<u8> = needle.iter().flat_map(|b| format!("{b:02X}").into_bytes()).collect();
    let seen = |hay: &[u8]| contains(hay, needle) || contains(&hay.to_ascii_uppercase(), &hex);
    let mut found = Vec::new();
    let check = |o: &Object, place: String, found: &mut Vec<String>| {
        let mut text = Vec::new();
        serialize(o, &mut text);
        if seen(&text) {
            found.push(format!("{place} (as written)"));
        }
        if let Object::Stream(s) = o
            && let Ok(decoded) = s.decoded()
            && seen(&decoded)
        {
            found.push(format!("{place} (decoded stream)"));
        }
    };
    // 1. The raw bytes.
    if seen(bytes) {
        found.push("raw bytes".into());
    }
    // 2. Every indirect object in the file, each parsed where it stands: finds objects that no
    //    cross-reference section lists any more.
    for at in object_headers(bytes) {
        if let Ok((r, o)) = parse_indirect(bytes, at, &|_| None) {
            check(&o, format!("object {} at byte {at}", r.num), &mut found);
        }
    }
    // 3. Every object of every revision, as a reader resolves it (decrypted, object streams
    //    unpacked).
    match Cos::open_with_password(Arc::new(bytes.to_vec()), password) {
        Ok(doc) => {
            for (n, end) in doc.revision_ends().into_iter().enumerate() {
                let Some(prefix) = bytes.get(..end) else { continue };
                let Ok(rev) = Cos::open_with_password(Arc::new(prefix.to_vec()), password) else {
                    found.push(format!("revision {} doesn't open on its own", n + 1));
                    continue;
                };
                for (r, o) in rev.scan_objects_checked() {
                    if let Ok(o) = o {
                        check(&o, format!("revision {} object {}", n + 1, r.num), &mut found);
                    }
                }
            }
        }
        Err(e) => found.push(format!("the saved file doesn't open: {e}")),
    }
    // 4. Extracted text.
    for (page, text) in extracted_text(bytes, password).into_iter().enumerate() {
        if text.contains(canary) {
            found.push(format!("text of page {}", page + 1));
        }
    }
    found
}

fn extracted_text(bytes: &[u8], password: Option<&str>) -> Vec<String> {
    let (s, id) = open(bytes.to_vec(), password);
    let doc = s.get(id).unwrap();
    let config = pdfcraft_render::RenderConfig { password: password.map(Arc::from), ..Default::default() };
    let mut r = pdfcraft_render::PageRenderer::new(doc.bytes.clone(), config);
    (0..doc.info.pages.len())
        .map(|page| {
            let req = pdfcraft_render::RenderRequest { page, kind: pdfcraft_render::RequestKind::Text, scale: 1.0, ..Default::default() };
            r.render(req).text.map(|t| t.plain_text()).unwrap_or_default()
        })
        .collect()
}

/// The saved file is one revision: no appended update carries the original along.
fn assert_single_revision(bytes: &[u8], password: Option<&str>) {
    let doc = Cos::open_with_password(Arc::new(bytes.to_vec()), password).unwrap();
    assert_eq!(doc.revisions().len(), 1, "one revision");
    assert_eq!(doc.revision_ends(), [bytes.len()], "nothing follows the only revision");
    assert!(doc.trailer().get(b"Prev").is_none(), "no /Prev chain");
    assert_eq!(bytes.windows(5).filter(|w| w == b"%%EOF").count(), 1, "one %%EOF");
}

fn line_index(s: &Session, id: DocId, page: usize, canary: &str) -> usize {
    s.get(id).unwrap().text_lines(page).iter().position(|l| l.text.contains(canary)).expect("the canary's line")
}

const FONT: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>";

/// One page with three lines of text, the middle one `canary`.
fn one_page(canary: &str) -> Vec<u8> {
    let body = format!("{}{}{}", line(250, "Opening line stays"), line(200, canary), line(150, "Closing line stays"));
    build(
        &[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [4 0 R] /Count 1 /MediaBox [0 0 300 300] >>".into(),
            FONT.into(),
            "<< /Type /Page /Parent 2 0 R /Contents 5 0 R /Resources << /Font << /F1 3 0 R >> >> >>".into(),
            stream(&body),
        ],
        "",
    )
}

#[test]
fn replaced_text_is_absent_from_the_saved_file() {
    const CANARY: &str = "OACANARY-REPLACED-7f3a91";
    let original = one_page(CANARY);
    assert!(!find_canary(&original, CANARY, None).is_empty(), "the inspection finds the canary before the edit");
    let (mut s, id) = open(original, None);
    let line = line_index(&s, id, 0, CANARY);
    s.apply(id, Edit::EditTextLine { page: 0, line, text: "Replacement wording".into() }).unwrap();
    // The working file keeps the old revision (in memory); the inspection would see it there.
    assert!(!find_canary(&s.get(id).unwrap().bytes, CANARY, None).is_empty(), "the working file still holds it");
    let saved = s.save_bytes(id).unwrap();
    assert_eq!(find_canary(&saved, CANARY, None), Vec::<String>::new());
    assert_single_revision(&saved, None);
    let text = extracted_text(&saved, None).concat();
    for kept in ["Opening line stays", "Replacement wording", "Closing line stays"] {
        assert!(text.contains(kept), "the rest of the page is intact: {text:?}");
    }
}

#[test]
fn deleted_text_and_a_deleted_page_are_not_recoverable() {
    const TEXT: &str = "OACANARY-DELETED-TEXT-2b4c";
    const PAGE: &str = "OACANARY-DELETED-PAGE-9e01";
    let page2 = format!("{}{}", line(200, PAGE), line(150, "Second page"));
    let original = build(
        &[
            "<< /Type /Catalog /Pages 2 0 R /Outlines 10 0 R >>".into(),
            "<< /Type /Pages /Kids [4 0 R 6 0 R 8 0 R] /Count 3 /MediaBox [0 0 300 300] >>".into(),
            FONT.into(),
            "<< /Type /Page /Parent 2 0 R /Contents 5 0 R /Resources << /Font << /F1 3 0 R >> >> >>".into(),
            stream(&format!("{}{}", line(250, "First page"), line(200, TEXT))),
            // The page to delete, with a comment and a bookmark pointing at it.
            "<< /Type /Page /Parent 2 0 R /Contents 7 0 R /Resources << /Font << /F1 3 0 R >> >> /Annots [12 0 R] >>".into(),
            stream(&page2),
            "<< /Type /Page /Parent 2 0 R /Contents 9 0 R /Resources << /Font << /F1 3 0 R >> >> >>".into(),
            stream(&line(250, "Third page")),
            "<< /Type /Outlines /First 11 0 R /Last 11 0 R /Count 1 >>".into(),
            "<< /Title (Second page) /Parent 10 0 R /Dest [6 0 R /Fit] >>".into(),
            format!("<< /Type /Annot /Subtype /Text /Rect [10 10 30 30] /Contents ({PAGE} note) /P 6 0 R >>"),
        ],
        "",
    );
    let (mut s, id) = open(original, None);
    let line = line_index(&s, id, 0, TEXT);
    s.apply(id, Edit::EditTextLine { page: 0, line, text: String::new() }).unwrap();
    s.apply(id, Edit::DeletePages { pages: vec![1] }).unwrap();
    let saved = s.save_bytes(id).unwrap();
    assert_eq!(find_canary(&saved, TEXT, None), Vec::<String>::new(), "deleted text");
    assert_eq!(find_canary(&saved, PAGE, None), Vec::<String>::new(), "deleted page");
    assert_single_revision(&saved, None);
    let text = extracted_text(&saved, None);
    assert_eq!(text.len(), 2);
    assert!(text[0].contains("First page") && text[1].contains("Third page"), "{text:?}");
}

/// An attachment (a file attachment comment), a document information entry and a form field's
/// value, removed by ordinary edits that don't themselves ask for a full rewrite.
#[test]
fn removed_attachments_metadata_and_form_values_are_absent() {
    const ATTACHMENT: &str = "OACANARY-ATTACHMENT-c3d2";
    const METADATA: &str = "OACANARY-METADATA-55aa";
    const FORM: &str = "OACANARY-FORM-VALUE-0b7e";
    let appearance = format!("/Tx BMC BT /Helv 10 Tf 2 5 Td ({FORM}) Tj ET EMC");
    let original = build(
        &[
            "<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [6 0 R] /DA (/Helv 0 Tf 0 g) /DR << /Font << /Helv 3 0 R >> >> >> >>".into(),
            "<< /Type /Pages /Kids [4 0 R] /Count 1 /MediaBox [0 0 300 300] >>".into(),
            FONT.into(),
            "<< /Type /Page /Parent 2 0 R /Contents 5 0 R /Resources << /Font << /F1 3 0 R >> >> /Annots [8 0 R 6 0 R] >>".into(),
            stream(&line(280, "A form")),
            format!(
                "<< /Type /Annot /Subtype /Widget /FT /Tx /T (client) /V ({FORM}) /DA (/Helv 10 Tf 0 g) /Rect [20 200 220 220] /P 4 0 R /AP << /N 7 0 R >> >>"
            ),
            format!(
                "<< /Type /XObject /Subtype /Form /BBox [0 0 200 20] /Resources << /Font << /Helv 3 0 R >> >> /Length {} >>\nstream\n{appearance}\nendstream",
                appearance.len()
            ),
            "<< /Type /Annot /Subtype /FileAttachment /Rect [250 250 270 270] /Name /PushPin /Contents (attached) /FS 9 0 R /P 4 0 R >>".into(),
            "<< /Type /Filespec /F (notes.txt) /UF (notes.txt) /EF << /F 10 0 R >> >>".into(),
            format!("<< /Type /EmbeddedFile /Length {} >>\nstream\n{ATTACHMENT}\nendstream", ATTACHMENT.len()),
            format!("<< /Title (Synthetic) /Subject ({METADATA}) >>"),
        ],
        "/Info 11 0 R",
    );
    let (mut s, id) = open(original, None);
    s.apply(id, Edit::DeleteAnnotation { page: 0, index: 0 }).unwrap();
    s.apply(id, Edit::SetInfo { key: "Subject".into(), value: String::new() }).unwrap();
    s.apply(id, Edit::ResetForm { names: None }).unwrap();
    let saved = s.save_bytes(id).unwrap();
    for canary in [ATTACHMENT, METADATA, FORM] {
        assert_eq!(find_canary(&saved, canary, None), Vec::<String>::new(), "{canary}");
    }
    assert_single_revision(&saved, None);
    let (s2, id2) = open(saved.to_vec(), None);
    assert_eq!(s2.get(id2).unwrap().info_value("Title").as_deref(), Some("Synthetic"), "other metadata stays");
}

/// Document-level attachments and XMP metadata, removed with Remove Hidden Information.
#[test]
fn hidden_information_removal_leaves_no_copy_behind() {
    const ATTACHMENT: &str = "OACANARY-NAMED-ATTACHMENT-77c1";
    const XMP: &str = "OACANARY-XMP-4e2f";
    let xmp = format!("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><dc:description>{XMP}</dc:description></x:xmpmeta>");
    let original = build(
        &[
            "<< /Type /Catalog /Pages 2 0 R /Metadata 6 0 R /Names << /EmbeddedFiles << /Names [(ledger.txt) 7 0 R] >> >> >>".into(),
            "<< /Type /Pages /Kids [4 0 R] /Count 1 /MediaBox [0 0 300 300] >>".into(),
            FONT.into(),
            "<< /Type /Page /Parent 2 0 R /Contents 5 0 R /Resources << /Font << /F1 3 0 R >> >> >>".into(),
            stream(&line(250, "Visible text")),
            format!("<< /Type /Metadata /Subtype /XML /Length {} >>\nstream\n{xmp}\nendstream", xmp.len()),
            "<< /Type /Filespec /F (ledger.txt) /EF << /F 8 0 R >> >>".into(),
            format!("<< /Type /EmbeddedFile /Length {} >>\nstream\n{ATTACHMENT}\nendstream", ATTACHMENT.len()),
        ],
        "",
    );
    let (mut s, id) = open(original, None);
    s.apply(id, Edit::RemoveHidden { which: vec![Hidden::Metadata, Hidden::Attachments] }).unwrap();
    let saved = s.save_bytes(id).unwrap();
    for canary in [ATTACHMENT, XMP] {
        assert_eq!(find_canary(&saved, canary, None), Vec::<String>::new(), "{canary}");
    }
    assert_single_revision(&saved, None);
}

/// A file whose objects sit in compressed object streams, with a page drawing a form XObject
/// that draws another (compressed) one holding the canary.
#[test]
fn nested_xobjects_and_object_streams_keep_no_deleted_canary() {
    const NESTED: &str = "OACANARY-NESTED-XOBJECT-d00d";
    const PACKED: &str = "OACANARY-OBJSTM-61f0";
    let classic = build(
        &[
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [4 0 R 6 0 R] /Count 2 /MediaBox [0 0 300 300] >>".into(),
            FONT.into(),
            "<< /Type /Page /Parent 2 0 R /Contents 5 0 R /Resources << /Font << /F1 3 0 R >> >> >>".into(),
            stream(&line(250, "Kept page")),
            "<< /Type /Page /Parent 2 0 R /Contents 7 0 R /Resources << /XObject << /Fx 8 0 R >> >> >>".into(),
            stream("q /Fx Do Q"),
            "<< /Type /XObject /Subtype /Form /BBox [0 0 300 300] /Resources << /XObject << /Fy 9 0 R >> >> /Length 10 >>\nstream\nq /Fy Do Q\nendstream".into(),
            "<< /Type /XObject /Subtype /Form /BBox [0 0 300 300] /Length 0 >>\nstream\n\nendstream".into(),
            format!("<< /Title (Packed) /Keywords ({PACKED}) >>"),
        ],
        "/Info 10 0 R",
    );
    // Compress the innermost form and pack every dictionary into object streams.
    let mut cos = Cos::open(Arc::new(classic)).unwrap();
    let inner = pdfcraft_cos::ObjRef::new(9, 0);
    let placeholder = cos.get(inner);
    let Object::Stream(empty) = &*placeholder else { panic!("a stream") };
    let mut dict = empty.dict.clone();
    dict.set(
        b"Resources".to_vec(),
        Object::Dict({
            let mut fonts = pdfcraft_cos::Dict::new();
            fonts.set(b"F1".to_vec(), Object::Ref(pdfcraft_cos::ObjRef::new(3, 0)));
            let mut res = pdfcraft_cos::Dict::new();
            res.set(b"Font".to_vec(), Object::Dict(fonts));
            res
        }),
    );
    cos.set(inner, Object::Stream(pdfcraft_cos::Stream::flate(dict, line(150, NESTED).as_bytes())));
    let original = pdfcraft_cos::write_full(&cos, &SaveOptions { object_streams: true, ..SaveOptions::default() }).unwrap();
    assert!(contains(&original, b"/ObjStm"), "the input uses object streams");
    assert!(!contains(&original, PACKED.as_bytes()), "the information dictionary is packed into a compressed object stream");
    for canary in [NESTED, PACKED] {
        assert!(!find_canary(&original, canary, None).is_empty(), "the inspection finds {canary} in the input");
    }
    let (mut s, id) = open(original, None);
    s.apply(id, Edit::DeletePages { pages: vec![1] }).unwrap();
    s.apply(id, Edit::SetInfo { key: "Keywords".into(), value: "public".into() }).unwrap();
    let saved = s.save_bytes(id).unwrap();
    for canary in [NESTED, PACKED] {
        assert_eq!(find_canary(&saved, canary, None), Vec::<String>::new(), "{canary}");
    }
    assert_single_revision(&saved, None);
}

/// Save to the file's own path (after an earlier save) and Save As both give the full rewrite:
/// the engine has one save path whatever the destination.
#[test]
fn saving_again_in_place_is_also_a_full_rewrite() {
    const FIRST: &str = "OACANARY-FIRST-SAVE-a1a1";
    const SECOND: &str = "OACANARY-SECOND-SAVE-b2b2";
    let (mut s, id) = open(one_page(FIRST), None);
    let line = line_index(&s, id, 0, FIRST);
    s.apply(id, Edit::EditTextLine { page: 0, line, text: SECOND.into() }).unwrap();
    let first = s.save_bytes(id).unwrap();
    s.mark_saved(id, first.clone(), Some("/tmp/oa/synthetic.pdf".into())).unwrap();
    assert_eq!(find_canary(&first, FIRST, None), Vec::<String>::new());
    let line = line_index(&s, id, 0, SECOND);
    s.apply(id, Edit::EditTextLine { page: 0, line, text: "Final wording".into() }).unwrap();
    let second = s.save_bytes(id).unwrap();
    assert!(!second.starts_with(&first), "not appended to the file as last saved");
    for canary in [FIRST, SECOND] {
        assert_eq!(find_canary(&second, canary, None), Vec::<String>::new(), "{canary}");
    }
    assert_single_revision(&second, None);
}

#[test]
fn an_unchanged_document_is_rewritten_too() {
    // A file another application saved incrementally: the old revision held the canary.
    const CANARY: &str = "OACANARY-THIRD-PARTY-HISTORY-3c3c";
    let mut cos = Cos::open(Arc::new(one_page(CANARY))).unwrap();
    let content = pdfcraft_cos::ObjRef::new(5, 0);
    cos.set(content, Object::Stream(pdfcraft_cos::Stream::from_raw(pdfcraft_cos::Dict::new(), line(200, "Current text").into_bytes())));
    let with_history = pdfcraft_cos::write_incremental(&cos, &SaveOptions::default()).unwrap();
    assert!(!find_canary(&with_history, CANARY, None).is_empty());
    let (s, id) = open(with_history, None);
    let saved = s.save_bytes(id).unwrap();
    assert_eq!(find_canary(&saved, CANARY, None), Vec::<String>::new());
    assert_single_revision(&saved, None);
}

fn encrypted(canary: &str) -> Vec<u8> {
    let mut doc = Cos::open(Arc::new(one_page(canary))).unwrap();
    doc.set_encryption(&pdfcraft_cos::NewEncryption {
        algorithm: pdfcraft_cos::Algorithm::Aes256,
        user_password: "user-pw",
        owner_password: "owner-pw",
        permissions: -1,
        encrypt_metadata: true,
        seed: [9; 32],
    })
    .unwrap();
    pdfcraft_cos::write_full(&doc, &SaveOptions::default()).unwrap()
}

#[test]
fn encrypted_documents_are_saved_encrypted_without_the_removed_text() {
    const CANARY: &str = "OACANARY-ENCRYPTED-8d8d";
    let original = encrypted(CANARY);
    assert!(!find_canary(&original, CANARY, Some("user-pw")).is_empty(), "decryption finds the canary before the edit");
    let (mut s, id) = open(original.clone(), Some("user-pw"));
    let line = line_index(&s, id, 0, CANARY);
    s.apply(id, Edit::EditTextLine { page: 0, line, text: "Encrypted replacement".into() }).unwrap();
    let saved = s.save_bytes(id).unwrap();
    let without = Cos::open(saved.clone()).err();
    assert!(
        matches!(without, Some(pdfcraft_cos::CosError::NeedsPassword | pdfcraft_cos::CosError::WrongPassword)),
        "still needs its password: {without:?}"
    );
    let before = Cos::open_with_password(Arc::new(original), Some("user-pw")).unwrap();
    let after = Cos::open_with_password(saved.clone(), Some("user-pw")).unwrap();
    assert_eq!(after.security().unwrap().dict(), before.security().unwrap().dict(), "the same encryption and permissions");
    assert_eq!(find_canary(&saved, CANARY, Some("user-pw")), Vec::<String>::new());
    assert_single_revision(&saved, Some("user-pw"));
    assert!(extracted_text(&saved, Some("user-pw")).concat().contains("Encrypted replacement"));
}

#[test]
fn a_rewrite_that_lost_its_encryption_is_refused() {
    let (s, id) = open(encrypted("OACANARY-UNUSED-0000"), Some("owner-pw"));
    let doc = s.get(id).unwrap();
    let editor = doc.editor.as_ref().unwrap();
    let mut plain = editor.cos.clone();
    plain.remove_encryption();
    let unencrypted = Arc::new(pdfcraft_cos::write_full(&plain, &SaveOptions::default()).unwrap());
    let err = verify_rewrite(&editor.cos, editor.keys.reopen.as_deref(), &unencrypted).unwrap_err();
    assert!(matches!(&err, EditError::Write(m) if m.contains("password-protected") && m.contains("nothing was saved")), "{err}");
    // Removing protection on purpose is the user's choice, and is saved unencrypted.
    let (mut s, id) = open(encrypted("OACANARY-UNUSED-0000"), Some("owner-pw"));
    s.apply(id, Edit::RemoveProtection).unwrap();
    assert!(Cos::open(s.save_bytes(id).unwrap()).is_ok());
}

fn digital_id() -> sign::DigitalId {
    let p12 = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../sign/tests/data/ec-p256.p12")).unwrap();
    sign::pkcs12::open(&p12, "test").unwrap()
}

#[test]
fn signing_an_edited_document_does_not_keep_the_removed_text() {
    const CANARY: &str = "OACANARY-BEFORE-SIGNING-5151";
    // After the test certificate's start date.
    let mut s = Session::new().with_clock(|| 1_800_000_000);
    let id = s.open("synthetic.pdf", None, Arc::new(one_page(CANARY)), None).unwrap();
    let line = line_index(&s, id, 0, CANARY);
    s.apply(id, Edit::EditTextLine { page: 0, line, text: "Signed wording".into() }).unwrap();
    let signed = s.sign(id, &digital_id(), SignOptions { page: 0, rect: Some([20.0, 20.0, 160.0, 50.0]), ..SignOptions::default() }).unwrap();
    assert_eq!(find_canary(&signed, CANARY, None), Vec::<String>::new());
    s.mark_signed(id, signed, None).unwrap();
    assert!(s.get(id).unwrap().is_signed());
}

#[test]
fn signed_documents_with_changes_are_refused_unless_signatures_may_be_invalidated() {
    let mut s = Session::new().with_clock(|| 1_800_000_000);
    let id = s.open("synthetic.pdf", None, Arc::new(one_page("Signed content")), None).unwrap();
    let signed = s.sign(id, &digital_id(), SignOptions { page: 0, rect: Some([20.0, 20.0, 160.0, 50.0]), ..SignOptions::default() }).unwrap();
    s.mark_signed(id, signed.clone(), None).unwrap();
    // Unchanged: Save gives the signed file exactly as it is.
    assert_eq!(s.save_bytes(id).unwrap(), signed);
    // Changed: refused, explaining why; nothing about the document changes.
    s.apply(id, Edit::SetInfo { key: "Title".into(), value: "Changed after signing".into() }).unwrap();
    let err = s.save_bytes(id).unwrap_err();
    assert_eq!(err, EditError::Signed);
    assert!(err.to_string().contains("invalidate its signatures") && err.to_string().contains("not saved"), "{err}");
    assert!(s.get(id).unwrap().dirty);
    assert_eq!(s.save_full_bytes(id).unwrap_err(), EditError::Signed);
    // Explicitly accepted: a single-revision rewrite, whose signature no longer covers the file.
    let rewritten = s.save_bytes_with(id, SignedSave::InvalidateSignatures).unwrap();
    assert!(!rewritten.starts_with(&signed));
    assert_single_revision(&rewritten, None);
    s.mark_saved(id, rewritten, None).unwrap();
    let sigs = &s.get(id).unwrap().signatures;
    assert!(sigs.iter().all(|sig| sig.status != SignatureStatus::Valid), "never reported valid after the rewrite: {sigs:?}");
}
