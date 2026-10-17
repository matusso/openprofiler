//! Remote targets on the command line: `--ssh`, `--docker`, `--kube` (docs/DESIGN.md §3.9).

use anyhow::{Context, Result, bail};
use openprofiler_daemon::live::AttachOptions;
use openprofiler_remote::{DEFAULT_DEBUG_IMAGE, Hop, Target};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(clap::Args, Clone, Default)]
pub struct RemoteArgs {
    /// Reach the JVM over SSH: `[user@]host[:port]`. Repeat to jump through bastion hosts; the last
    /// one is the target host.
    #[arg(long, value_name = "[USER@]HOST[:PORT]")]
    ssh: Vec<String>,
    /// Identity file for the SSH target host.
    #[arg(long, requires = "ssh", value_name = "FILE")]
    ssh_identity: Option<PathBuf>,
    /// Extra `ssh -o` option for the SSH target host, e.g. `StrictHostKeyChecking=accept-new`.
    #[arg(long, requires = "ssh", value_name = "OPTION")]
    ssh_option: Vec<String>,
    /// The JVM runs in this Docker container (on the SSH host, if given).
    #[arg(long, value_name = "CONTAINER", conflicts_with = "kube")]
    docker: Option<String>,
    /// The JVM runs in this Kubernetes pod.
    #[arg(long, value_name = "[NAMESPACE/]POD[/CONTAINER]")]
    kube: Option<String>,
    /// kubectl context.
    #[arg(long, requires = "kube")]
    context: Option<String>,
    /// Profile through an ephemeral debug container (for distroless images without a shell),
    /// optionally with this image.
    #[arg(long, requires = "kube", value_name = "IMAGE", num_args = 0..=1,
          default_missing_value = DEFAULT_DEBUG_IMAGE)]
    debug_container: Option<String>,
    /// Directory with the remote bundle (`<triple>/openprofiler-relay`, agent libraries).
    #[arg(long, env = "OPENPROFILER_BUNDLE_DIR")]
    bundle_dir: Option<PathBuf>,
}

impl RemoteArgs {
    /// The remote target, or `None` for the local machine.
    pub fn target(&self) -> Result<Option<Target>> {
        let mut t = Target::local();
        if let Some((host, jump)) = self.ssh.split_last() {
            let mut hop = Hop::ssh(host, jump.to_vec())?;
            if let Hop::Ssh { identity, extra_args, .. } = &mut hop {
                identity.clone_from(&self.ssh_identity);
                extra_args.extend(self.ssh_option.iter().flat_map(|o| ["-o".to_owned(), o.clone()]));
            }
            t = t.then(hop);
        }
        if let Some(c) = &self.docker {
            t = t.then(Hop::Docker { container: c.clone(), user: None });
        }
        if let Some(k) = &self.kube {
            t = t.then(Hop::kubectl(k, self.context.clone(), self.debug_container.clone())?);
        }
        Ok((!t.hops.is_empty()).then_some(t))
    }

    pub fn attach_options(&self, interval: Option<String>) -> Result<AttachOptions> {
        Ok(AttachOptions {
            interval,
            remote: self.target()?,
            bundle_dir: self.bundle_dir.clone(),
            ..Default::default()
        })
    }
}

/// JVMs on the target, in the `/jvms` row format.
pub async fn list(opts: &AttachOptions) -> Result<Vec<Value>> {
    let v = match &opts.remote {
        Some(t) => {
            let mut req = json!({ "target": t, "what": "jvms" });
            if let Some(d) = &opts.bundle_dir {
                req["options"] = json!({ "bundle_dir": d });
            }
            openprofiler_daemon::live::remote_discover(req).await?
        }
        None => serde_json::to_value(openprofiler_daemon::live::list_jvms()?)?,
    };
    Ok(v.as_array().cloned().unwrap_or_default())
}

/// `pid`, or the only attachable JVM on the target.
pub async fn pick_pid(pid: Option<u32>, opts: &AttachOptions) -> Result<u32> {
    if let Some(p) = pid {
        return Ok(p);
    }
    let jvms = list(opts).await?;
    let attachable: Vec<&Value> = jvms.iter().filter(|j| j["attachable"] != false).collect();
    match attachable.as_slice() {
        [j] => j["pid"].as_u64().map(|p| p as u32).context("JVM without pid"),
        [] => bail!("no attachable JVM found"),
        many => bail!(
            "{} JVMs found, pass a pid:\n{}",
            many.len(),
            many.iter()
                .map(|j| format!("  {}  {}", j["pid"], j["main_class"].as_str().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

/// Streams an HPROF heap dump of a remote JVM into `output`.
pub async fn dump(target: &Target, opts: &AttachOptions, pid: u32, output: &std::path::Path) -> Result<u64> {
    let target = target.resolve().await?;
    let bundle =
        openprofiler_remote::ensure_bundle(&target, &openprofiler_daemon::live::bundle_dir(opts)).await?;
    openprofiler_remote::dump_heap(&target, &bundle.relay, pid, output).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        remote: RemoteArgs,
    }

    fn target(args: &[&str]) -> Option<Target> {
        let mut argv = vec!["x"];
        argv.extend(args);
        Cli::try_parse_from(argv).unwrap().remote.target().unwrap()
    }

    #[test]
    fn targets_from_flags() {
        assert!(target(&[]).is_none());
        assert_eq!(
            target(&["--ssh", "bastion", "--ssh", "me@box", "--docker", "app"]).unwrap().describe(),
            "ssh:box → docker:app"
        );
        assert_eq!(
            target(&["--kube", "prod/api-0", "--debug-container"]).unwrap().describe(),
            "k8s:prod/api-0 (debug)"
        );
        let t = target(&["--kube", "api-0", "--debug-container", "alpine:3"]).unwrap();
        assert!(matches!(&t.hops[0], Hop::Kubectl { debug_image: Some(i), .. } if i == "alpine:3"));
        assert!(Cli::try_parse_from(["x", "--docker", "a", "--kube", "b"]).is_err());
        assert!(Cli::try_parse_from(["x", "--debug-container"]).is_err());
        let t = target(&["--ssh", "box", "--ssh-option", "StrictHostKeyChecking=no", "--ssh-identity", "k"])
            .unwrap();
        assert!(matches!(&t.hops[0], Hop::Ssh { identity: Some(_), extra_args, .. }
            if extra_args == &["-o", "StrictHostKeyChecking=no"]));
    }
}
