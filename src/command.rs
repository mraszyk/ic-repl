use super::error::pretty_parse;
use super::exp::Exp;
use super::helper::{did_to_canister_info, FileSource, MyHelper};
use super::token::{ParserError, Tokenizer};
use super::utils::{get_dfx_hsm_pin, resolve_path};
use anyhow::{anyhow, Context};
use candid::{types::value::IDLValue, Principal, TypeEnv};
use candid_parser::configs::Configs;
use pretty_assertions::{assert_eq, assert_ne};
use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct Commands(pub Vec<(Command, Range<usize>)>);
#[derive(Debug, Clone)]
pub enum Command {
    Config(String),
    Show(Exp),
    Let(String, Exp),
    Assert(BinOp, Exp, Exp),
    Import(String, Principal, Option<String>),
    Load(Exp),
    Identity(String, IdentityConfig),
    Func {
        name: String,
        args: Vec<String>,
        body: Vec<Command>,
    },
    While {
        cond: Exp,
        body: Vec<Command>,
    },
    If {
        cond: Exp,
        then: Vec<Command>,
        else_: Vec<Command>,
    },
}
#[derive(Debug, Clone)]
pub enum IdentityConfig {
    Empty,
    Pem(String),
    Hsm { slot_index: usize, key_id: String },
}
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone)]
pub enum BinOp {
    Equal,
    SubEqual,
    NotEqual,
}

impl Command {
    pub fn run(self, helper: &mut MyHelper) -> anyhow::Result<()> {
        match self {
            Command::Import(id, canister_id, did) => {
                if let Some(did) = &did {
                    let path = resolve_path(&helper.base_path, did);
                    let info = did_to_canister_info(did, FileSource::Path(&path), None)?;
                    helper.canister_map.borrow_mut().0.insert(canister_id, info);
                }
                // TODO decide if it's a Service instead
                helper.env.0.insert(id, IDLValue::Principal(canister_id));
            }
            Command::Let(id, val) => {
                let is_call = val.is_call();
                let v = val.eval(helper)?;
                bind_value(helper, id, v, is_call, false);
            }
            Command::Func { name, args, body } => {
                helper.func_env.0.insert(name, (args, body));
            }
            Command::Assert(op, left, right) => {
                let left = left.eval(helper)?;
                let right = right.eval(helper)?;
                match op {
                    BinOp::Equal => assert_eq!(left, right),
                    BinOp::SubEqual => {
                        if let (IDLValue::Text(left), IDLValue::Text(right)) = (&left, &right) {
                            assert!(left.contains(right));
                        } else {
                            let l_ty = left.value_ty();
                            let r_ty = right.value_ty();
                            let env = TypeEnv::new();
                            if let Ok(left) = left.annotate_type(false, &env, &r_ty) {
                                assert_eq!(left, right);
                            } else if let Ok(right) = right.annotate_type(false, &env, &l_ty) {
                                assert_eq!(left, right);
                            } else {
                                assert_eq!(left, right);
                            }
                        }
                    }
                    BinOp::NotEqual => assert_ne!(left, right),
                }
            }
            Command::Config(conf) => {
                if conf.ends_with(".toml") {
                    let path = resolve_path(&helper.base_path, &conf);
                    let conf = std::fs::read_to_string(path)?;
                    helper.config = conf.parse::<Configs>()?;
                } else {
                    helper.config = conf.parse::<Configs>()?;
                }
            }
            Command::Show(val) => {
                let is_call = val.is_call();
                let time = Instant::now();
                let v = val.eval(helper)?;
                let duration = time.elapsed();
                bind_value(helper, "_".to_string(), v, is_call, true);
                if helper.verbose {
                    let width = console::Term::stdout().size().1 as usize;
                    println!("{:>width$}", format!("({duration:.2?})"), width = width);
                }
            }
            Command::Identity(id, config) => {
                use ic_agent::identity::{BasicIdentity, Identity, Secp256k1Identity};
                let identity: Arc<dyn Identity> = match &config {
                    IdentityConfig::Hsm { slot_index, key_id } => {
                        #[cfg(target_os = "macos")]
                        const PKCS11_LIBPATH: &str = "/Library/OpenSC/lib/pkcs11/opensc-pkcs11.so";
                        #[cfg(target_os = "linux")]
                        const PKCS11_LIBPATH: &str = "/usr/lib/x86_64-linux-gnu/opensc-pkcs11.so";
                        #[cfg(target_os = "windows")]
                        const PKCS11_LIBPATH: &str =
                            "C:/Program Files/OpenSC Project/OpenSC/pkcs11/opensc-pkcs11.dll";
                        let lib_path = std::env::var("PKCS11_LIBPATH")
                            .unwrap_or_else(|_| PKCS11_LIBPATH.to_string());
                        Arc::from(ic_identity_hsm::HardwareIdentity::new(
                            lib_path,
                            *slot_index,
                            key_id,
                            get_dfx_hsm_pin,
                        )?)
                    }
                    IdentityConfig::Pem(pem_path) => {
                        let pem_path = resolve_path(&helper.base_path, pem_path);
                        match Secp256k1Identity::from_pem_file(&pem_path) {
                            Ok(identity) => Arc::from(identity),
                            Err(_) => Arc::from(BasicIdentity::from_pem_file(&pem_path)?),
                        }
                    }
                    IdentityConfig::Empty => match helper.identity_map.0.get(&id) {
                        Some(identity) => identity.clone(),
                        None => Arc::from(BasicIdentity::from_signing_key(
                            ed25519_consensus::SigningKey::new(rand::thread_rng()),
                        )),
                    },
                };
                helper
                    .identity_map
                    .0
                    .insert(id.to_string(), identity.clone());
                let sender = identity.sender().map_err(|e| anyhow!("{}", e))?;
                println!("Current identity {sender}");

                helper.agent.set_arc_identity(identity.clone());
                helper.current_identity = id.to_string();
                helper.env.0.insert(id, IDLValue::Principal(sender));
            }
            Command::Load(e) => {
                // TODO check for infinite loop
                // Note that it's a bit tricky to make load as a built-in function, as it requires mutable access to helper.
                let IDLValue::Text(file) = e.eval(helper)? else {
                    return Err(anyhow!("load needs to be a file path"));
                };
                let (file, fail_safe) = if file.ends_with('?') {
                    (file.trim_end_matches('?'), true)
                } else {
                    (file.as_str(), false)
                };
                let old_base = helper.base_path.clone();
                let path = resolve_path(&old_base, file);
                let read_result = std::fs::read_to_string(&path);
                if read_result.is_err() && fail_safe {
                    return Ok(());
                }
                let mut script = read_result.with_context(|| format!("Cannot read {path:?}"))?;
                if script.starts_with("#!") {
                    let line_end = script.find('\n').unwrap_or(0);
                    script.drain(..line_end);
                }
                let script =
                    shellexpand::env(&script).map_err(|e| crate::token::error2(e, 0..0))?;
                let cmds = pretty_parse::<Commands>(file, &script)?;
                helper.base_path = path.parent().unwrap().to_path_buf();
                for (cmd, pos) in cmds.0.into_iter() {
                    if helper.verbose {
                        println!("> {}", &script[pos]);
                    }
                    cmd.run(helper)?;
                }
                helper.base_path = old_base;
            }
            Command::If { cond, then, else_ } => {
                let IDLValue::Bool(cond) = cond.eval(helper)? else {
                    return Err(anyhow!("if condition is not a boolean expression"));
                };
                if cond {
                    for cmd in then.into_iter() {
                        cmd.run(helper)?;
                    }
                } else {
                    for cmd in else_.into_iter() {
                        cmd.run(helper)?;
                    }
                }
            }
            Command::While { cond, body } => loop {
                let IDLValue::Bool(cond) = cond.clone().eval(helper)? else {
                    return Err(anyhow!("while condition is not a boolean expression"));
                };
                if !cond {
                    break;
                }
                for cmd in body.iter() {
                    cmd.clone().run(helper)?;
                }
            },
        }
        Ok(())
    }
}

impl std::str::FromStr for Command {
    type Err = ParserError;
    fn from_str(str: &str) -> Result<Self, Self::Err> {
        let lexer = Tokenizer::new(str);
        super::grammar::CommandParser::new().parse(lexer)
    }
}
impl std::str::FromStr for Commands {
    type Err = ParserError;
    fn from_str(str: &str) -> Result<Self, Self::Err> {
        let lexer = Tokenizer::new(str);
        super::grammar::CommandsParser::new().parse(lexer)
    }
}

fn bind_value(helper: &mut MyHelper, id: String, v: IDLValue, is_call: bool, display: bool) {
    if display {
        if helper.verbose {
            println!("{v}");
        } else if let IDLValue::Text(v) = &v {
            println!("{v}");
        }
    }
    if is_call {
        let (v, cost) = crate::profiling::may_extract_profiling(v);
        if let Some(cost) = cost {
            let cost_id = format!("__cost_{id}");
            helper.env.0.insert(cost_id, IDLValue::Int64(cost));
        }
        helper.env.0.insert(id, v);
    } else {
        helper.env.0.insert(id, v);
    }
}
