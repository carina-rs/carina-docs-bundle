use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use carina_docs_bundle::{
    run_docs_render_worker, DocsCoordinate, DocsRenderLimits, DocsRendererError,
    SubprocessDocsRenderer, DOCS_ARCHIVE_MAX_BYTES, DOCS_RENDER_WORKER_COMMAND,
};

const USAGE: &str = "Usage: carina-docs-bundle check <archive.tar.gz> [options]

Options:
  --namespace <namespace>          Default: local
  --name <provider>                Default: provider
  --version <version>              Default: 0.0.0
  -h, --help                      Show this help

The checker fully validates and renders in a subprocess using registry defaults:
384 MiB, 20 CPU seconds, 30 seconds wall time. For registry-equivalent verdicts,
use the deployed revision and coordinates. Linux enforces an address-space limit;
macOS uses the formatter, CPU and wall-clock limits.
Exit codes: 0 valid, 1 invalid/render-aborted, 2 usage/I/O/infrastructure.
";

struct Check {
    archive: PathBuf,
    coordinate: DocsCoordinate,
}

fn main() {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let command = arguments.next();
    // The synchronous worker runs before a runtime, tracing or configuration exists.
    if command.as_deref() == Some(OsStr::new(DOCS_RENDER_WORKER_COMMAND)) {
        if let Err(error) = run_docs_render_worker(arguments) {
            eprintln!("docs render worker failed: {error:#}");
            std::process::exit(error.exit_code());
        }
        return;
    }
    if matches!(
        command.as_deref().and_then(OsStr::to_str),
        Some("--help" | "-h")
    ) {
        print!("{USAGE}");
        return;
    }
    if command.as_deref() != Some(OsStr::new("check")) {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let check = match parse_check(arguments) {
        Ok(Some(check)) => check,
        Ok(None) => {
            print!("{USAGE}");
            return;
        }
        Err(error) => {
            eprintln!("{error}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    std::process::exit(run_check(check));
}

fn parse_check(mut arguments: impl Iterator<Item = OsString>) -> Result<Option<Check>, String> {
    let mut archive = None;
    let mut namespace = "local".to_string();
    let mut name = "provider".to_string();
    let mut version = "0.0.0".to_string();
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--help" | "-h") => return Ok(None),
            Some("--namespace" | "--name" | "--version") => {
                let option = argument.to_str().expect("matched UTF-8 option");
                let value = arguments
                    .next()
                    .ok_or_else(|| format!("{option} requires a value"))?
                    .into_string()
                    .map_err(|_| format!("{option} requires UTF-8"))?;
                match option {
                    "--namespace" => namespace = value,
                    "--name" => name = value,
                    "--version" => version = value,
                    _ => unreachable!(),
                }
            }
            Some(value) if value.starts_with('-') => {
                return Err(format!("unknown option: {value}"))
            }
            _ if archive.is_none() => archive = Some(PathBuf::from(argument)),
            _ => return Err("check accepts exactly one archive".to_string()),
        }
    }
    Ok(Some(Check {
        archive: archive.ok_or("check requires an archive")?,
        coordinate: DocsCoordinate::new(&namespace, &name, &version)
            .map_err(|error| error.to_string())?,
    }))
}

fn run_check(check: Check) -> i32 {
    let archive = (|| -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        File::open(&check.archive)?
            .take(DOCS_ARCHIVE_MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    })();
    let archive = match archive {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("could not read {}: {error}", check.archive.display());
            return 2;
        }
    };
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("could not locate documentation checker: {error}");
            return 2;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("could not initialize documentation checker: {error}");
            return 2;
        }
    };
    let renderer = SubprocessDocsRenderer::new(executable, DocsRenderLimits::default());
    match runtime.block_on(renderer.validate_and_render(archive, &check.coordinate)) {
        Ok(_) => {
            println!("documentation bundle is valid");
            0
        }
        Err(error) => {
            let code = match &error {
                DocsRendererError::InvalidBundle(_) | DocsRendererError::RenderAborted(_) => 1,
                DocsRendererError::Infrastructure { .. } => 2,
            };
            eprintln!("{error}");
            code
        }
    }
}
