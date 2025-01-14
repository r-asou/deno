use crate::colors;
use crate::flags::Flags;
use crate::tokio_util;
use crate::version;
use deno_core::error::bail;
use deno_core::error::type_error;
use deno_core::error::AnyError;
use deno_core::futures::FutureExt;
use deno_core::ModuleLoader;
use deno_core::ModuleSpecifier;
use deno_core::OpState;
use deno_runtime::permissions::Permissions;
use deno_runtime::worker::MainWorker;
use deno_runtime::worker::WorkerOptions;
use std::cell::RefCell;
use std::convert::TryInto;
use std::env::current_exe;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

const MAGIC_TRAILER: &[u8; 8] = b"d3n0l4nd";

/// This function will try to run this binary as a standalone binary
/// produced by `deno compile`. It determines if this is a stanalone
/// binary by checking for the magic trailer string `D3N0` at EOF-12.
/// After the magic trailer is a u64 pointer to the start of the JS
/// file embedded in the binary. This file is read, and run. If no
/// magic trailer is present, this function exits with Ok(()).
pub fn try_run_standalone_binary(args: Vec<String>) -> Result<(), AnyError> {
  let current_exe_path = current_exe()?;

  let mut current_exe = File::open(current_exe_path)?;
  let trailer_pos = current_exe.seek(SeekFrom::End(-16))?;
  let mut trailer = [0; 16];
  current_exe.read_exact(&mut trailer)?;
  let (magic_trailer, bundle_pos_arr) = trailer.split_at(8);
  if magic_trailer == MAGIC_TRAILER {
    let bundle_pos_arr: &[u8; 8] = bundle_pos_arr.try_into()?;
    let bundle_pos = u64::from_be_bytes(*bundle_pos_arr);
    current_exe.seek(SeekFrom::Start(bundle_pos))?;

    let bundle_len = trailer_pos - bundle_pos;
    let mut bundle = String::new();
    current_exe.take(bundle_len).read_to_string(&mut bundle)?;
    // TODO: check amount of bytes read

    if let Err(err) = tokio_util::run_basic(run(bundle, args)) {
      eprintln!("{}: {}", colors::red_bold("error"), err.to_string());
      std::process::exit(1);
    }
    std::process::exit(0);
  } else {
    Ok(())
  }
}

const SPECIFIER: &str = "file://$deno$/bundle.js";

struct EmbeddedModuleLoader(String);

impl ModuleLoader for EmbeddedModuleLoader {
  fn resolve(
    &self,
    _op_state: Rc<RefCell<OpState>>,
    specifier: &str,
    _referrer: &str,
    _is_main: bool,
  ) -> Result<ModuleSpecifier, AnyError> {
    if specifier != SPECIFIER {
      return Err(type_error(
        "Self-contained binaries don't support module loading",
      ));
    }
    Ok(ModuleSpecifier::resolve_url(specifier)?)
  }

  fn load(
    &self,
    _op_state: Rc<RefCell<OpState>>,
    module_specifier: &ModuleSpecifier,
    _maybe_referrer: Option<ModuleSpecifier>,
    _is_dynamic: bool,
  ) -> Pin<Box<deno_core::ModuleSourceFuture>> {
    let module_specifier = module_specifier.clone();
    let code = self.0.to_string();
    async move {
      if module_specifier.to_string() != SPECIFIER {
        return Err(type_error(
          "Self-contained binaries don't support module loading",
        ));
      }
      Ok(deno_core::ModuleSource {
        code,
        module_url_specified: module_specifier.to_string(),
        module_url_found: module_specifier.to_string(),
      })
    }
    .boxed_local()
  }
}

async fn run(source_code: String, args: Vec<String>) -> Result<(), AnyError> {
  let flags = Flags {
    argv: args[1..].to_vec(),
    // TODO(lucacasonato): remove once you can specify this correctly through embedded metadata
    unstable: true,
    ..Default::default()
  };
  let main_module = ModuleSpecifier::resolve_url(SPECIFIER)?;
  let permissions = Permissions::allow_all();
  let module_loader = Rc::new(EmbeddedModuleLoader(source_code));
  let create_web_worker_cb = Arc::new(|_| {
    todo!("Worker are currently not supported in standalone binaries");
  });

  let options = WorkerOptions {
    apply_source_maps: false,
    args: flags.argv.clone(),
    debug_flag: false,
    user_agent: crate::http_util::get_user_agent(),
    unstable: true,
    ca_filepath: None,
    seed: None,
    js_error_create_fn: None,
    create_web_worker_cb,
    attach_inspector: false,
    maybe_inspector_server: None,
    should_break_on_first_statement: false,
    module_loader,
    runtime_version: version::deno(),
    ts_version: version::TYPESCRIPT.to_string(),
    no_color: !colors::use_color(),
    get_error_class_fn: Some(&crate::errors::get_error_class_name),
  };
  let mut worker =
    MainWorker::from_options(main_module.clone(), permissions, &options);
  worker.bootstrap(&options);
  worker.execute_module(&main_module).await?;
  worker.execute("window.dispatchEvent(new Event('load'))")?;
  worker.run_event_loop().await?;
  worker.execute("window.dispatchEvent(new Event('unload'))")?;
  Ok(())
}

/// This functions creates a standalone deno binary by appending a bundle
/// and magic trailer to the currently executing binary.
pub async fn create_standalone_binary(
  mut source_code: Vec<u8>,
  output: PathBuf,
) -> Result<(), AnyError> {
  let original_binary_path = std::env::current_exe()?;
  let mut original_bin = tokio::fs::read(original_binary_path).await?;

  let mut trailer = MAGIC_TRAILER.to_vec();
  trailer.write_all(&original_bin.len().to_be_bytes())?;

  let mut final_bin =
    Vec::with_capacity(original_bin.len() + source_code.len() + trailer.len());
  final_bin.append(&mut original_bin);
  final_bin.append(&mut source_code);
  final_bin.append(&mut trailer);

  let output =
    if cfg!(windows) && output.extension().unwrap_or_default() != "exe" {
      PathBuf::from(output.display().to_string() + ".exe")
    } else {
      output
    };

  if output.exists() {
    // If the output is a directory, throw error
    if output.is_dir() {
      bail!("Could not compile: {:?} is a directory.", &output);
    }

    // Make sure we don't overwrite any file not created by Deno compiler.
    // Check for magic trailer in last 16 bytes
    let mut output_file = File::open(&output)?;
    output_file.seek(SeekFrom::End(-16))?;
    let mut trailer = [0; 16];
    output_file.read_exact(&mut trailer)?;
    let (magic_trailer, _) = trailer.split_at(8);
    if magic_trailer != MAGIC_TRAILER {
      bail!("Could not compile: cannot overwrite {:?}.", &output);
    }
  }
  tokio::fs::write(&output, final_bin).await?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o777);
    tokio::fs::set_permissions(output, perms).await?;
  }

  Ok(())
}
