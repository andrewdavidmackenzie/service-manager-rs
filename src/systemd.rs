use crate::utils::wrap_output;

use super::{
    utils, ServiceInstallCtx, ServiceLevel, ServiceManager, ServiceStartCtx, ServiceStopCtx,
    ServiceUninstallCtx,
};
use std::{
    fmt, io,
    path::PathBuf,
    process::{Command, Output, Stdio},
};

static SYSTEMCTL: &str = "systemctl";
const SERVICE_FILE_PERMISSIONS: u32 = 0o644;

/// Configuration settings tied to systemd services
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemdConfig {
    pub install: SystemdInstallConfig,
}

/// Configuration settings tied to systemd services during installation
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemdInstallConfig {
    pub start_limit_interval_sec: Option<u32>,
    pub start_limit_burst: Option<u32>,
    pub restart: SystemdServiceRestartType,
    pub restart_sec: Option<u32>,
}

impl Default for SystemdInstallConfig {
    fn default() -> Self {
        Self {
            start_limit_interval_sec: None,
            start_limit_burst: None,
            restart: SystemdServiceRestartType::OnFailure,
            restart_sec: None,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum SystemdServiceRestartType {
    No,
    Always,
    OnSuccess,
    OnFailure,
    OnAbnormal,
    OnAbort,
    OnWatch,
}

impl Default for SystemdServiceRestartType {
    fn default() -> Self {
        Self::No
    }
}

impl fmt::Display for SystemdServiceRestartType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::No => write!(f, "no"),
            Self::Always => write!(f, "always"),
            Self::OnSuccess => write!(f, "on-success"),
            Self::OnFailure => write!(f, "on-failure"),
            Self::OnAbnormal => write!(f, "on-abnormal"),
            Self::OnAbort => write!(f, "on-abort"),
            Self::OnWatch => write!(f, "on-watch"),
        }
    }
}

/// Implementation of [`ServiceManager`] for Linux's [systemd](https://en.wikipedia.org/wiki/Systemd)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemdServiceManager {
    /// Whether or not this manager is operating at the user-level
    pub user: bool,

    /// Configuration settings tied to systemd services
    pub config: SystemdConfig,
}

impl SystemdServiceManager {
    /// Creates a new manager instance working with system services
    pub fn system() -> Self {
        Self::default()
    }

    /// Creates a new manager instance working with user services
    pub fn user() -> Self {
        Self::default().into_user()
    }

    /// Change manager to work with system services
    pub fn into_system(self) -> Self {
        Self {
            config: self.config,
            user: false,
        }
    }

    /// Change manager to work with user services
    pub fn into_user(self) -> Self {
        Self {
            config: self.config,
            user: true,
        }
    }

    /// Update manager to use the specified config
    pub fn with_config(self, config: SystemdConfig) -> Self {
        Self {
            config,
            user: self.user,
        }
    }
}

impl ServiceManager for SystemdServiceManager {
    fn available(&self) -> io::Result<bool> {
        match which::which(SYSTEMCTL) {
            Ok(_) => Ok(true),
            Err(which::Error::CannotFindBinaryPath) => Ok(false),
            Err(x) => Err(io::Error::new(io::ErrorKind::Other, x)),
        }
    }

    fn install(&self, ctx: ServiceInstallCtx) -> io::Result<()> {
        let dir_path = if self.user {
            systemd_user_dir_path()?
        } else {
            systemd_global_dir_path()
        };

        std::fs::create_dir_all(&dir_path)?;

        let script_name = ctx.label.to_script_name();
        let script_path = dir_path.join(format!("{script_name}.service"));
        let service = match ctx.contents {
            Some(contents) => contents,
            _ => make_service(
                &self.config.install,
                &script_name,
                &ctx,
                self.user,
                ctx.autostart,
                ctx.disable_restart_on_failure,
                ctx.requires_network,
            ),
        };

        utils::write_file(
            script_path.as_path(),
            service.as_bytes(),
            SERVICE_FILE_PERMISSIONS,
        )?;

        if ctx.autostart {
            wrap_output(systemctl(
                "enable",
                script_path.to_string_lossy().as_ref(),
                self.user,
            )?)?;
        }

        Ok(())
    }

    fn uninstall(&self, ctx: ServiceUninstallCtx) -> io::Result<()> {
        let dir_path = if self.user {
            systemd_user_dir_path()?
        } else {
            systemd_global_dir_path()
        };
        let script_name = ctx.label.to_script_name();
        let script_path = dir_path.join(format!("{script_name}.service"));

        wrap_output(systemctl(
            "disable",
            script_path.to_string_lossy().as_ref(),
            self.user,
        )?)?;
        std::fs::remove_file(script_path)
    }

    fn start(&self, ctx: ServiceStartCtx) -> io::Result<()> {
        wrap_output(systemctl("start", &ctx.label.to_script_name(), self.user)?)?;
        Ok(())
    }

    fn stop(&self, ctx: ServiceStopCtx) -> io::Result<()> {
        wrap_output(systemctl("stop", &ctx.label.to_script_name(), self.user)?)?;
        Ok(())
    }

    fn level(&self) -> ServiceLevel {
        if self.user {
            ServiceLevel::User
        } else {
            ServiceLevel::System
        }
    }

    fn set_level(&mut self, level: ServiceLevel) -> io::Result<()> {
        match level {
            ServiceLevel::System => self.user = false,
            ServiceLevel::User => self.user = true,
        }

        Ok(())
    }

    fn status(&self, ctx: crate::ServiceStatusCtx) -> io::Result<crate::ServiceStatus> {
        let output = systemctl("status", &ctx.label.to_script_name(), self.user)?;
        // ref: https://www.freedesktop.org/software/systemd/man/latest/systemctl.html#Exit%20status
        match output.status.code() {
            Some(4) => Ok(crate::ServiceStatus::NotInstalled),
            Some(3) => Ok(crate::ServiceStatus::Stopped(None)),
            Some(0) => Ok(crate::ServiceStatus::Running),
            _ => Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "Command failed with exit code {}: {}",
                    output.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stderr)
                ),
            )),
        }
    }
}

fn systemctl(cmd: &str, label: &str, user: bool) -> io::Result<Output> {
    let mut command = Command::new(SYSTEMCTL);

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if user {
        command.arg("--user");
    }

    command.arg(cmd).arg(label).output()
}

#[inline]
pub fn systemd_global_dir_path() -> PathBuf {
    PathBuf::from("/etc/systemd/system")
}

pub fn systemd_user_dir_path() -> io::Result<PathBuf> {
    Ok(dirs::config_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Unable to locate home directory"))?
        .join("systemd")
        .join("user"))
}

fn make_service(
    config: &SystemdInstallConfig,
    description: &str,
    ctx: &ServiceInstallCtx,
    user: bool,
    autostart: bool,
    disable_restart_on_failure: bool,
    requires_network: bool,
) -> String {
    use std::fmt::Write as _;
    let SystemdInstallConfig {
        start_limit_interval_sec,
        start_limit_burst,
        restart,
        restart_sec,
    } = config;

    let mut service = String::new();
    let _ = writeln!(service, "[Unit]");
    let _ = writeln!(service, "Description={description}");

    if requires_network {
        // delay the start of this service until after networking has been started
        let _ = writeln!(service, "After=network-online.target");
        // this service requires that networking be up and online before it is started
        let _ = writeln!(service, "Requires=network-online.target");
    }

    if let Some(x) = start_limit_interval_sec {
        let _ = writeln!(service, "StartLimitIntervalSec={x}");
    }

    if let Some(x) = start_limit_burst {
        let _ = writeln!(service, "StartLimitBurst={x}");
    }

    let _ = writeln!(service, "[Service]");
    if let Some(working_directory) = &ctx.working_directory {
        let _ = writeln!(
            service,
            "WorkingDirectory={}",
            working_directory.to_string_lossy()
        );
    }

    if let Some(env_vars) = &ctx.environment {
        for (var, val) in env_vars {
            let _ = writeln!(service, "Environment=\"{var}={val}\"");
        }
    }

    let program = ctx.program.to_string_lossy();
    let args = ctx
        .args
        .clone()
        .into_iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect::<Vec<String>>()
        .join(" ");
    let _ = writeln!(service, "ExecStart={program} {args}");

    if !disable_restart_on_failure {
        if *restart != SystemdServiceRestartType::No {
            let _ = writeln!(service, "Restart={restart}");
        }

        if let Some(x) = restart_sec {
            let _ = writeln!(service, "RestartSec={x}");
        }
    }

    // For Systemd, a user-mode service definition should *not* specify the username, since it runs
    // as the current user. The service will not start correctly if the definition specifies the
    // username, even if it's the same as the current user. The option for specifying a user really
    // only applies for a system-level service that doesn't run as root.
    if !user {
        if let Some(username) = &ctx.username {
            let _ = writeln!(service, "User={username}");
        }
    }

    if user && autostart {
        let _ = writeln!(service, "[Install]");
        let _ = writeln!(service, "WantedBy=default.target");
    } else if autostart {
        let _ = writeln!(service, "[Install]");
        let _ = writeln!(service, "WantedBy=multi-user.target");
    }

    service.trim().to_string()
}
