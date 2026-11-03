#![cfg(unix)]

mod common;

use std::process::{Command, Output, Stdio};

use common::{bundle, output_overflow, ArchiveFile};

const CHECKER: &str = env!("CARGO_BIN_EXE_carina-docs-bundle");
const RESOURCE_FAILURE: &str =
    "documentation rendering exceeded the registry's resource limits or failed";

fn check(archive: &[u8], options: &[&str]) -> Output {
    let file = ArchiveFile::new(archive);
    Command::new(CHECKER)
        .arg("check")
        .arg(file.path())
        .args([
            "--namespace",
            "carina-rs",
            "--name",
            "aws",
            "--version",
            "1.2.3",
        ])
        .args(options)
        .output()
        .unwrap()
}

#[test]
fn valid_archive_passes() {
    let output = check(&bundle(&[("index.md", b"# Provider\n")]), &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("valid"));
}

#[test]
fn prerelease_version_is_rejected() {
    let output = check(
        &bundle(&[("index.md", b"# Provider\n")]),
        &["--version", "1.2.3-rc.1"],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("pre-release versions cannot include documentation"));
    assert!(output.stdout.is_empty());
}

#[test]
fn invalid_semver_is_rejected() {
    let output = check(
        &bundle(&[("index.md", b"# Provider\n")]),
        &["--version", "1.2"],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid version:"));
    assert!(output.stdout.is_empty());
}

#[test]
fn version_build_metadata_is_rejected() {
    let output = check(
        &bundle(&[("index.md", b"# Provider\n")]),
        &["--version", "1.2.3+build"],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("build metadata"));
    assert!(output.stdout.is_empty());
}

#[test]
fn invalid_namespace_is_rejected() {
    let output = check(
        &bundle(&[("index.md", b"# Provider\n")]),
        &["--namespace", "bad/namespace"],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid namespace:"));
    assert!(output.stdout.is_empty());
}

#[test]
fn invalid_name_is_rejected() {
    let output = check(
        &bundle(&[("index.md", b"# Provider\n")]),
        &["--name", "bad/name"],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid name:"));
    assert!(output.stdout.is_empty());
}

#[test]
fn exactly_256_kib_passes() {
    let mut markdown = b"# Provider\n\n".to_vec();
    // Pad one paragraph without adding lines or pipes.
    markdown.resize(256 * 1024 - 1, b'a');
    markdown.push(b'\n');
    let output = check(&bundle(&[("index.md", &markdown)]), &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn one_byte_over_256_kib_fails_with_the_protocol_limit_message() {
    let mut markdown = b"# Provider\n\n".to_vec();
    markdown.resize(256 * 1024, b'a');
    markdown.push(b'\n');
    let output = check(&bundle(&[("index.md", &markdown)]), &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("file exceeds the 256 KiB limit"));
    assert!(output.stdout.is_empty());
}

#[test]
fn formatter_output_overflow_is_refused_after_archive_validation() {
    let output = check(&output_overflow(), &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("rendered documentation page exceeds the 2 MiB limit"));
    assert!(output.stdout.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_address_space_overflow_is_a_bundle_refusal() {
    let archive = bundle(&[("index.md", "a@b.c ".repeat(20_000).as_bytes())]);
    let output = check(&archive, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains(RESOURCE_FAILURE));
    assert!(output.stdout.is_empty());
}

#[test]
fn cpu_heavy_markdown_is_contained() {
    let mut markdown = b"# test.Resource\n".to_vec();
    markdown.resize(256 * 1024, b'&');
    let output = check(
        &bundle(&[
            ("index.md", &markdown),
            ("resources/a.md", &markdown),
            ("resources/b.md", &markdown),
            ("resources/c.md", &markdown),
        ]),
        &[],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains(RESOURCE_FAILURE));
}

#[tokio::test]
async fn wall_timeout_kills_and_reaps_the_checker_worker() {
    use carina_docs_bundle::{
        DocsCoordinate, DocsRenderLimits, DocsRendererError, RenderAbort, SubprocessDocsRenderer,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    let script = ArchiveFile::new(b"");
    let pid_file = ArchiveFile::new(b"");
    let pid_path = pid_file.path().to_str().unwrap().replace('\'', "'\\''");
    std::fs::write(
        script.path(),
        format!(
        "#!/bin/sh\nprintf '%s' \"$$\" > '{pid_path}'\n/bin/cat >/dev/null\nexec /bin/sleep 600\n"
    ),
    )
    .unwrap();
    std::fs::set_permissions(script.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let renderer = SubprocessDocsRenderer::new(
        script.path(),
        DocsRenderLimits::with_timeout(384 * 1024 * 1024, 20, Duration::from_secs(1)),
    );
    let coordinate = DocsCoordinate::new("local", "provider", "0.0.0").unwrap();
    let rendered = renderer
        .validate_and_render(bundle(&[("index.md", b"# Index\n")]), &coordinate)
        .await;
    assert!(matches!(
        rendered,
        Err(DocsRendererError::RenderAborted(RenderAbort::TimedOut))
    ));
    let pid: libc::pid_t = std::fs::read_to_string(pid_file.path())
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only checks the child PID; it does not signal another process.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "timed-out worker is still present"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn worker_setup_failure_has_its_reserved_exit_code() {
    let output = Command::new(CHECKER)
        .arg("render-docs-worker")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(70));
    assert!(String::from_utf8_lossy(&output.stderr).contains("worker has no memory limit"));
}

#[test]
fn usage_and_io_errors_are_distinct_from_bundle_refusals() {
    let output = Command::new(CHECKER).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let output = Command::new(CHECKER)
        .args(["check", "/no-such-carina-docs-archive.tar.gz"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn resource_limit_overrides_are_rejected() {
    let archive = bundle(&[("index.md", b"# Provider\n")]);
    for option in ["--memory-limit-bytes", "--cpu-seconds", "--timeout-seconds"] {
        let output = check(&archive, &[option, "1"]);
        assert_eq!(output.status.code(), Some(2), "{option}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&format!("unknown option: {option}"))
        );
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn compressed_input_is_bounded_before_rendering() {
    let output = check(&vec![0; 16 * 1024 * 1024 + 1], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("16 MiB"));
    assert!(output.stdout.is_empty());
}
