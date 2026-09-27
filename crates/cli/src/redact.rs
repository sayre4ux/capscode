use std::io::{self, Write};

use tracing::Level;

pub fn init_tracing(verbose: bool) {
    let level = if verbose { Level::DEBUG } else { Level::INFO };
    let _ = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .without_time()
        .with_writer(RedactingStderr)
        .try_init();
}

struct RedactingStderr;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RedactingStderr {
    type Writer = RedactingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter
    }
}

struct RedactingWriter;

impl Write for RedactingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let input = String::from_utf8_lossy(buf);
        io::stderr().write_all(redact(&input).as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

pub(crate) fn redact(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(index) = rest.find("sk-") {
        output.push_str(&rest[..index]);
        output.push_str("[REDACTED]");
        let secret = &rest[index..];
        let end = secret
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '\'' | '"' | ',' | ']' | '}'))
            .unwrap_or(secret.len());
        rest = &secret[end..];
    }
    output.push_str(rest);

    for label in ["PRIVATE KEY", "WIREGUARD PRIVATE KEY"] {
        let begin = format!("-----BEGIN {label}-----");
        let end = format!("-----END {label}-----");
        while let Some(start) = output.find(&begin) {
            let Some(relative_end) = output[start..].find(&end) else {
                output.replace_range(start.., "[REDACTED PRIVATE KEY]");
                break;
            };
            let finish = start + relative_end + end.len();
            output.replace_range(start..finish, "[REDACTED PRIVATE KEY]");
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn secrets_are_redacted() {
        let rendered =
            redact("key=sk-live-secret next -----BEGIN PRIVATE KEY-----x-----END PRIVATE KEY-----");
        assert!(!rendered.contains("sk-live-secret"));
        assert!(!rendered.contains("-----BEGIN PRIVATE KEY-----"));
    }
}
