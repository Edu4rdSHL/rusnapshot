use {
    crate::{args::Args, structs::ExtraArgs},
    anyhow::Result,
    std::path::Path,
};

#[must_use]
pub fn is_same_character(s: &str, specific_char: char) -> bool {
    s.chars().all(|c| c == specific_char)
}

pub fn check_creation_requirements(arguments: &mut Args, extra_args: &ExtraArgs) -> Result<()> {
    arguments.check_for_source_and_dest_dir();

    arguments.init(extra_args)?;

    // It's required to have a trailing slash for the source and destination directories.
    // Otherwise, when we retrieve the snapshot data from the database, we won't be able to
    // restore/delete the snapshot because the source/destination paths won't match.
    if !arguments.source_dir.is_empty() && !arguments.source_dir.ends_with('/') {
        arguments.source_dir += "/";
    }
    if !arguments.dest_dir.is_empty() && !arguments.dest_dir.ends_with('/') {
        arguments.dest_dir += "/";
    }

    // We need to make sure that the paths for source and destination are full paths.
    // If they are not, we will use the current working directory to build the full path.
    if !Path::new(&arguments.source_dir).is_absolute() {
        arguments.source_dir = std::env::current_dir()?
            .join(&arguments.source_dir)
            .to_str()
            .expect("Failed to get the source directory")
            .to_string();
    }

    if !Path::new(&arguments.dest_dir).is_absolute() {
        arguments.dest_dir = std::env::current_dir()?
            .join(&arguments.dest_dir)
            .to_str()
            .expect("Failed to get the destination directory")
            .to_string();
    }

    if arguments.machine.is_empty() {
        arguments.machine = hostname::get()?.to_string_lossy().to_string();
    }

    Ok(())
}
