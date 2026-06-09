use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use wasmtime::{Config, Engine, InstancePre, Linker, Module, Store};
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::{DirPerms, FilePerms, I32Exit, WasiCtxBuilder};

#[derive(Clone)]
pub struct WasmToolOptions {
    dirs: Vec<WasiDir>,
    env: Vec<(String, String)>,
    fuel: u64,
    output_limit: usize,
}

#[derive(Clone)]
struct WasiDir {
    host: PathBuf,
    guest: String,
    read_only: bool,
}

pub struct WasmTool {
    name: String,
    path: PathBuf,
    engine: Engine,
    pre: InstancePre<WasiP1Ctx>,
    options: WasmToolOptions,
}

impl WasmToolOptions {
    pub fn from_cli(
        rw_dirs: &[String],
        ro_dirs: &[String],
        env: &[String],
        inherit_env: &[String],
        fuel: u64,
        output_limit: usize,
    ) -> Result<Self> {
        if fuel == 0 {
            bail!("--tool-wasi-fuel must be greater than 0");
        }
        if output_limit == 0 {
            bail!("--tool-wasi-output-limit must be greater than 0");
        }

        let mut dirs = Vec::with_capacity(rw_dirs.len() + ro_dirs.len());
        for spec in rw_dirs {
            dirs.push(parse_wasi_dir(spec, false)?);
        }
        for spec in ro_dirs {
            dirs.push(parse_wasi_dir(spec, true)?);
        }
        let mut guest_paths = HashSet::new();
        for dir in &dirs {
            if !guest_paths.insert(dir.guest.clone()) {
                bail!("duplicate Wasm tool WASI guest path: {}", dir.guest);
            }
        }

        let mut merged_env = Vec::with_capacity(env.len() + inherit_env.len());
        for spec in env {
            merged_env.push(parse_env_pair(spec)?);
        }
        for key in inherit_env {
            if key.is_empty() {
                bail!("--tool-wasi-env-inherit key must not be empty");
            }
            let value = std::env::var(key)
                .with_context(|| format!("inherit environment variable {key}"))?;
            merged_env.push((key.clone(), value));
        }

        Ok(Self {
            dirs,
            env: merged_env,
            fuel,
            output_limit,
        })
    }

    pub fn has_capabilities(
        rw_dirs: &[String],
        ro_dirs: &[String],
        env: &[String],
        inherit_env: &[String],
    ) -> bool {
        !rw_dirs.is_empty() || !ro_dirs.is_empty() || !env.is_empty() || !inherit_env.is_empty()
    }
}

impl WasmTool {
    pub fn load_all(specs: &[String], options: &WasmToolOptions) -> Result<Vec<Self>> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config)?;
        let mut seen = HashSet::new();
        let mut tools = Vec::with_capacity(specs.len());

        for spec in specs {
            let (name, path) = parse_tool_spec(spec)?;
            if !seen.insert(name.clone()) {
                bail!("duplicate --tool name: {name}");
            }
            let path = std::fs::canonicalize(&path)
                .with_context(|| format!("canonicalize Wasm tool {}", path.display()))?;
            let module = Module::from_file(&engine, &path)
                .with_context(|| format!("compile Wasm tool {}", path.display()))?;
            let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
            preview1::add_to_linker_sync(&mut linker, |ctx| ctx)?;
            let pre = linker
                .instantiate_pre(&module)
                .with_context(|| format!("link Wasm tool {}", path.display()))?;
            tools.push(Self {
                name,
                path,
                engine: engine.clone(),
                pre,
                options: options.clone(),
            });
        }

        Ok(tools)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn invoke(&self, args: Value) -> Result<Value> {
        let request = serde_json::json!({ "name": self.name, "args": args });
        let stdin = serde_json::to_vec(&request)?;
        let stdout = MemoryOutputPipe::new(self.options.output_limit);
        let stderr = MemoryOutputPipe::new(self.options.output_limit);

        let mut builder = WasiCtxBuilder::new();
        builder
            .allow_blocking_current_thread(true)
            .arg(self.path.to_string_lossy())
            .arg(&self.name)
            .stdin(MemoryInputPipe::new(stdin))
            .stdout(stdout.clone())
            .stderr(stderr.clone());

        for (key, value) in &self.options.env {
            builder.env(key, value);
        }
        for dir in &self.options.dirs {
            let dir_perms = if dir.read_only {
                DirPerms::READ
            } else {
                DirPerms::all()
            };
            let file_perms = if dir.read_only {
                FilePerms::READ
            } else {
                FilePerms::all()
            };
            builder
                .preopened_dir(&dir.host, &dir.guest, dir_perms, file_perms)
                .with_context(|| format!("preopen {} as {}", dir.host.display(), dir.guest))?;
        }

        let wasi = builder.build_p1();
        let mut store = Store::new(&self.engine, wasi);
        store.set_fuel(self.options.fuel)?;

        let instance = self.pre.instantiate(&mut store)?;
        let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
        let status = match start.call(&mut store, ()) {
            Ok(()) => 0,
            Err(err) => {
                if let Some(exit) = err.downcast_ref::<I32Exit>() {
                    exit.0
                } else {
                    let stderr_text = pipe_text(&stderr);
                    if stderr_text.trim().is_empty() {
                        return Err(err)
                            .with_context(|| format!("Wasm tool {} trapped", self.name));
                    }
                    return Err(err).with_context(|| {
                        format!(
                            "Wasm tool {} trapped; stderr: {}",
                            self.name,
                            stderr_text.trim()
                        )
                    });
                }
            }
        };

        let stdout_text = pipe_text(&stdout);
        let stderr_text = pipe_text(&stderr);
        if status != 0 {
            if stderr_text.trim().is_empty() {
                bail!("Wasm tool {} exited with status {status}", self.name);
            }
            bail!(
                "Wasm tool {} exited with status {status}; stderr: {}",
                self.name,
                stderr_text.trim()
            );
        }

        parse_tool_stdout(&self.name, &stdout_text)
    }
}

fn parse_tool_spec(spec: &str) -> Result<(String, PathBuf)> {
    let (name, path) = spec
        .split_once('=')
        .ok_or_else(|| anyhow!("--tool must use NAME=WASM syntax: {spec}"))?;
    let name = name.trim();
    if name.is_empty() {
        bail!("--tool name must not be empty: {spec}");
    }
    if name.starts_with("__") || name.starts_with("fs_") || name.starts_with("net_") {
        bail!("--tool name {name} is reserved");
    }
    if path.is_empty() {
        bail!("--tool path must not be empty: {spec}");
    }
    Ok((name.to_string(), PathBuf::from(path)))
}

fn parse_wasi_dir(spec: &str, read_only: bool) -> Result<WasiDir> {
    let (host, guest) = if let Some(idx) = spec.rfind(':') {
        let (host, guest) = spec.split_at(idx);
        let guest = &guest[1..];
        if guest.starts_with('/') || guest == "." || guest.starts_with("./") {
            (host, guest)
        } else {
            (spec, "/host")
        }
    } else {
        (spec, "/host")
    };

    if host.is_empty() {
        bail!("WASI preopen host path must not be empty: {spec}");
    }
    if guest.is_empty() {
        bail!("WASI preopen guest path must not be empty: {spec}");
    }

    let host = std::fs::canonicalize(host)
        .with_context(|| format!("canonicalize WASI preopen host path {host}"))?;
    Ok(WasiDir {
        host,
        guest: guest.to_string(),
        read_only,
    })
}

fn parse_env_pair(spec: &str) -> Result<(String, String)> {
    let (key, value) = spec
        .split_once('=')
        .ok_or_else(|| anyhow!("--tool-wasi-env must use KEY=VALUE syntax: {spec}"))?;
    if key.is_empty() {
        bail!("--tool-wasi-env key must not be empty: {spec}");
    }
    Ok((key.to_string(), value.to_string()))
}

fn pipe_text(pipe: &MemoryOutputPipe) -> String {
    String::from_utf8_lossy(&pipe.contents()).into_owned()
}

fn parse_tool_stdout(name: &str, stdout: &str) -> Result<Value> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Value::Null);
    }

    let value: Value = serde_json::from_str(trimmed)
        .with_context(|| format!("Wasm tool {name} wrote non-JSON stdout"))?;
    if let Some(object) = value.as_object() {
        if object.len() == 1 {
            if let Some(result) = object.get("result") {
                return Ok(result.clone());
            }
            if let Some(error) = object.get("error") {
                if let Some(message) = error.as_str() {
                    bail!("Wasm tool {name}: {message}");
                }
                bail!("Wasm tool {name}: {error}");
            }
        }
    }
    Ok(value)
}
