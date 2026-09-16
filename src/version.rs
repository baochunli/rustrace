use std::io::Write;

/// Version identity shared by production session metadata and CLI reporting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VersionMetadata {
    pub client_version: &'static str,
    pub build_identity: &'static str,
    pub event_format: u32,
    /// Provenance package (.rprov) container version.
    pub package_format: u32,
    /// Highest assignment package (.rta) version this binary accepts.
    pub assignment_format: u32,
}

impl VersionMetadata {
    pub fn build_commit(self) -> &'static str {
        let commit = self
            .build_identity
            .split_once(';')
            .map_or(self.build_identity, |(commit, _)| commit);
        match commit.strip_suffix("-dirty").unwrap_or(commit) {
            "" | "development-build-unavailable" | "source-archive-commit-unavailable" => "unknown",
            _ => commit,
        }
    }

    pub fn target(self) -> &'static str {
        self.build_identity
            .split_once(';')
            .map(|(_, target)| target)
            .filter(|target| !target.is_empty())
            .unwrap_or("unknown")
    }
}

/// Returns the compile-time identity persisted by every newly started session.
pub fn version_metadata() -> VersionMetadata {
    VersionMetadata {
        client_version: env!("CARGO_PKG_VERSION"),
        build_identity: option_env!("RUSTRACE_BUILD_ID").unwrap_or("development-build-unavailable"),
        event_format: rustrace_model::FORMAT_VERSION_V1,
        package_format: rustrace_model::RPROV_FORMAT_VERSION_V1,
        assignment_format: rustrace_model::assignment::SUPPORTED_FORMAT_VERSION,
    }
}

pub(crate) fn write_version<W: Write>(output: &mut W, verbose: bool) {
    write_version_metadata(output, version_metadata(), verbose);
}

fn write_version_metadata<W: Write>(output: &mut W, metadata: VersionMetadata, verbose: bool) {
    let _ = writeln!(output, "rustrace {}", metadata.client_version);
    if verbose {
        let _ = writeln!(output, "build commit: {}", metadata.build_commit());
        let _ = writeln!(output, "event format: {}", metadata.event_format);
        let _ = writeln!(output, "package format: {}", metadata.package_format);
        let _ = writeln!(output, "assignment format: {}", metadata.assignment_format);
        let _ = writeln!(output, "target: {}", metadata.target());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_exact_short_and_verbose_lines_for_an_unavailable_commit() {
        let metadata = VersionMetadata {
            client_version: env!("CARGO_PKG_VERSION"),
            build_identity: "source-archive-commit-unavailable-dirty;aarch64-apple-darwin",
            event_format: 1,
            package_format: 1,
            assignment_format: 2,
        };
        let mut output = Vec::new();

        write_version_metadata(&mut output, metadata, false);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("rustrace {}\n", env!("CARGO_PKG_VERSION"))
        );

        let mut output = Vec::new();
        write_version_metadata(&mut output, metadata, true);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!(
                "rustrace {}\nbuild commit: unknown\nevent format: 1\npackage format: 1\nassignment format: 2\ntarget: aarch64-apple-darwin\n",
                env!("CARGO_PKG_VERSION")
            )
        );
    }
}
