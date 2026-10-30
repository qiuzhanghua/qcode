use super::*;

/// The default: run the command as-is, subject to approval.
#[derive(Debug, Default)]
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn with_roots(
        &self,
        _roots: &[Root],
        _temp: Option<&Path>,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        Ok(Arc::new(NoSandbox))
    }

    fn name(&self) -> &'static str {
        "none"
    }

    fn describe(&self) -> String {
        "no OS sandbox; approval is the execution boundary".to_string()
    }

    fn command(
        &self,
        program: &OsStr,
        args: &[OsString],
        cwd: &Path,
    ) -> Result<Command, SandboxError> {
        let mut command = Command::new(program);
        command.args(args).current_dir(cwd);
        Ok(command)
    }
}
