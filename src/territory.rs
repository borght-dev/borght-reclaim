use std::path::{Component, Path};

/// Whose files a folder holds, judged from where it sits on the drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Territory {
    Projects,
    /// A program's own files under a user profile (`AppData`, `.vscode`, `.claude`): what
    /// looks like build output or dependencies there is what the program runs on.
    ToolHome,
    /// Windows itself and installed programs.
    InstalledSoftware,
}

const INSTALLED_SOFTWARE_FOLDERS: [&str; 4] =
    ["Windows", "Program Files", "Program Files (x86)", "ProgramData"];
const USER_PROFILES_FOLDER: &str = "Users";
const APPLICATION_DATA_FOLDER: &str = "AppData";

/// How many folders below the drive root a territory can start (`Users\<name>\AppData`).
pub const DEEPEST_START: usize = 3;

impl Territory {
    /// The territory that begins at exactly this folder; `Projects` when none does.
    pub fn starting_at(folder: &Path) -> Territory {
        match folders_below_drive(folder).as_slice() {
            [software] if is_installed_software_folder(software) => Territory::InstalledSoftware,
            [profiles, _, home]
                if profiles.eq_ignore_ascii_case(USER_PROFILES_FOLDER)
                    && (home.eq_ignore_ascii_case(APPLICATION_DATA_FOLDER)
                        || home.starts_with('.')) =>
            {
                Territory::ToolHome
            }
            _ => Territory::Projects,
        }
    }
}

pub fn depth_below_drive(folder: &Path) -> usize {
    folders_below_drive(folder).len()
}

pub fn is_inside_installed_software(path: &Path) -> bool {
    folders_below_drive(path).first().is_some_and(|top| is_installed_software_folder(top))
}

/// Top-level folders the machine cannot run without, however long they sat untouched.
pub fn is_operating_system_folder(folder: &Path) -> bool {
    matches!(
        folders_below_drive(folder).as_slice(),
        [top] if is_installed_software_folder(top) || top.eq_ignore_ascii_case(USER_PROFILES_FOLDER)
    )
}

fn is_installed_software_folder(name: &str) -> bool {
    INSTALLED_SOFTWARE_FOLDERS.iter().any(|folder| folder.eq_ignore_ascii_case(name))
}

fn folders_below_drive(path: &Path) -> Vec<&str> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_and_program_folders_are_installed_software() {
        for folder in
            [r"C:\Windows", r"C:\Program Files", r"c:\program files (x86)", r"E:\ProgramData"]
        {
            assert_eq!(Territory::starting_at(Path::new(folder)), Territory::InstalledSoftware);
        }
        assert!(is_inside_installed_software(Path::new(r"C:\Program Files\cursor\resources\app")));
        assert!(!is_inside_installed_software(Path::new(r"D:\Dev\Windows\bin")));
    }

    #[test]
    fn application_data_and_dot_folders_of_a_profile_are_tool_homes() {
        for folder in
            [r"C:\Users\someone\AppData", r"C:\Users\someone\.vscode", r"C:\users\someone\.claude"]
        {
            assert_eq!(Territory::starting_at(Path::new(folder)), Territory::ToolHome);
        }
    }

    #[test]
    fn everything_else_is_project_space() {
        for folder in [
            r"C:\Users\someone\Dev",
            r"C:\Users\someone",
            r"D:\Dev\repository\.vscode",
            r"D:\.worktrees",
            r"C:\Users\someone\AppData\Local",
        ] {
            assert_eq!(Territory::starting_at(Path::new(folder)), Territory::Projects, "{folder}");
        }
    }

    #[test]
    fn the_profiles_folder_is_an_operating_system_folder_but_its_contents_are_not() {
        assert!(is_operating_system_folder(Path::new(r"C:\Users")));
        assert!(is_operating_system_folder(Path::new(r"C:\Windows")));
        assert!(!is_operating_system_folder(Path::new(r"C:\Users\someone")));
        assert!(!is_operating_system_folder(Path::new(r"D:\Dev")));
    }
}
