use super::*;

/// The default: run the command as-is, subject to approval.
#[derive(Debug, Default)]
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn with_roots(
        &self,
        _roots: &[Root],
        _temp: Option<&Path>,
        _outside: OutsideRoots,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        Ok(Arc::new(NoSandbox))
    }

    fn name(&self) -> &'static str {
        "none"
    }

    fn describe(&self) -> String {
        qcode_i18n::tr("core.sandbox.describe.noop").into_owned()
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
