//! Document text extraction: PDF and DOCX.
//!
//! Provides two public functions for extracting plain text from document files.
//! Used by the agent as additional tools (`read_pdf`, `read_docx`).

use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;

/// Extract plain text from a PDF file.
pub fn extract_pdf_text(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read PDF: {}", path.display()))?;
    let text = pdf_extract::extract_text_from_mem(&bytes)
        .with_context(|| format!("failed to extract text from PDF: {}", path.display()))?;
    Ok(text)
}

/// Extract plain text from a DOCX file.
///
/// A DOCX file is a ZIP archive containing `word/document.xml`.
/// We parse the XML to extract text from `<w:t>` elements, joining
/// paragraphs with newlines.
pub fn extract_docx_text(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("failed to open DOCX: {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("failed to open DOCX as ZIP: {}", path.display()))?;

    let mut doc_xml = String::new();
    archive
        .by_name("word/document.xml")
        .context("DOCX missing word/document.xml — not a valid .docx file?")?
        .read_to_string(&mut doc_xml)
        .context("failed to read word/document.xml")?;

    let text = parse_docx_xml(&doc_xml)?;
    Ok(text)
}

/// Parse the word/document.xml to extract plain text.
///
/// DOCX stores text in `<w:t>` elements inside `<w:r>` (run) inside `<w:p>` (paragraph).
/// We separate paragraphs with newlines.
fn parse_docx_xml(xml: &str) -> Result<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);

    let mut buf = Vec::new();
    let mut paragraphs = Vec::new();
    let mut current_para = String::new();
    let mut in_t = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if e.local_name().as_ref() == b"t" {
                    in_t = true;
                }
                // <w:p> starts a new paragraph
                if e.local_name().as_ref() == b"p" && !current_para.is_empty() {
                    paragraphs.push(std::mem::take(&mut current_para));
                }
            }
            Ok(Event::End(ref e)) => {
                if e.local_name().as_ref() == b"t" {
                    in_t = false;
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_t {
                    if let Ok(text) = e.unescape() {
                        current_para.push_str(&text);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                // quick-xml may error on some files; try to recover
                if let quick_xml::Error::EndEventMismatch { .. } = e {
                    // skip malformed events
                    buf.clear();
                    continue;
                }
                return Err(anyhow::anyhow!("XML parse error in DOCX: {e}"));
            }
            _ => {}
        }
        buf.clear();
    }

    if !current_para.is_empty() {
        paragraphs.push(current_para);
    }

    if paragraphs.is_empty() {
        return Ok("(no text content found in DOCX)".to_string());
    }

    Ok(paragraphs.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn extract_text_from_docx() {
        let dir = std::env::temp_dir().join("tools-doc-test-docx");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.docx");
        create_test_docx(&path);
        let text = extract_docx_text(&path).unwrap();
        assert!(text.contains("Hello World"));
        assert!(text.contains("This is a test document"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Create a minimal valid .docx file for testing.
    fn create_test_docx(path: &std::path::Path) {
        use zip::write::FileOptions;
        use zip::ZipWriter;

        let file = std::fs::File::create(path).unwrap();
        let mut zip = ZipWriter::new(file);
        let options = FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        // Minimal [Content_Types].xml
        zip.start_file("[Content_Types].xml", options).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#,
        )
        .unwrap();

        // word/document.xml with text
        zip.start_file("word/document.xml", options).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>Hello World</w:t></w:r></w:p>
    <w:p><w:r><w:t>This is a test document.</w:t></w:r></w:p>
    <w:p><w:r><w:t xml:space="preserve">Multiple </w:t><w:t>runs in one paragraph.</w:t></w:r></w:p>
  </w:body>
</w:document>"#,
        )
        .unwrap();

        zip.finish().unwrap();
    }
}
