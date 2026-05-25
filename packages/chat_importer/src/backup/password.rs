#[derive(Debug, Default)]
pub struct PasswordManager {
    known_passwords: Vec<String>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PasswordInput {
    Password(String),
    Skip,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PasswordResolution<T> {
    Unlocked(T),
    Skip,
}

impl PasswordManager {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub fn known_passwords(&self) -> &[String] {
        &self.known_passwords
    }

    pub fn remember(&mut self, password: String) {
        if !self.known_passwords.iter().any(|known| known == &password) {
            self.known_passwords.push(password);
        }
    }

    pub fn parse_input(input: &str) -> PasswordInput {
        if input.trim().eq_ignore_ascii_case("skip") {
            PasswordInput::Skip
        } else {
            PasswordInput::Password(input.to_string())
        }
    }

    pub fn unlock_with<T, E, U, P, R>(
        &mut self,
        mut unlock: U,
        mut prompt: P,
        is_retryable: R,
    ) -> Result<PasswordResolution<T>, E>
    where
        U: FnMut(&str) -> Result<T, E>,
        P: FnMut() -> Result<PasswordInput, E>,
        R: Fn(&E) -> bool,
    {
        for password in self.known_passwords.clone() {
            match unlock(&password) {
                Ok(unlocked) => return Ok(PasswordResolution::Unlocked(unlocked)),
                Err(error) if is_retryable(&error) => {}
                Err(error) => return Err(error),
            }
        }

        loop {
            match prompt()? {
                PasswordInput::Skip => return Ok(PasswordResolution::Skip),
                PasswordInput::Password(password) => match unlock(&password) {
                    Ok(unlocked) => {
                        self.remember(password);
                        return Ok(PasswordResolution::Unlocked(unlocked));
                    }
                    Err(error) if is_retryable(&error) => {
                        self.remember(password);
                    }
                    Err(error) => return Err(error),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_passwords_once_in_order() {
        let mut manager = PasswordManager::new();
        manager.remember("first".into());
        manager.remember("second".into());
        manager.remember("first".into());

        assert_eq!(
            manager.known_passwords(),
            &["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn parses_skip_input_case_insensitively() {
        assert_eq!(PasswordManager::parse_input(" skip "), PasswordInput::Skip);
        assert_eq!(
            PasswordManager::parse_input("secret"),
            PasswordInput::Password("secret".into())
        );
    }

    #[test]
    fn retries_known_passwords_before_prompting() {
        let mut manager = PasswordManager::new();
        manager.remember("wrong".into());
        manager.remember("right".into());
        let mut attempts = Vec::new();
        let result = manager
            .unlock_with(
                |password| {
                    attempts.push(password.to_string());
                    (password == "right").then_some("ok").ok_or(())
                },
                || panic!("prompt should not be used after known password succeeds"),
                |_| true,
            )
            .unwrap();

        assert_eq!(result, PasswordResolution::Unlocked("ok"));
        assert_eq!(attempts, vec!["wrong".to_string(), "right".to_string()]);
    }

    #[test]
    fn remembers_new_password_after_prompt_success() {
        let mut manager = PasswordManager::new();
        manager.remember("old".into());
        let mut prompts = vec![
            PasswordInput::Password("bad".into()),
            PasswordInput::Password("new".into()),
        ]
        .into_iter();

        let result = manager
            .unlock_with(
                |password| (password == "new").then_some("ok").ok_or(()),
                || Ok(prompts.next().unwrap()),
                |_| true,
            )
            .unwrap();

        assert_eq!(result, PasswordResolution::Unlocked("ok"));
        assert_eq!(
            manager.known_passwords(),
            &["old".to_string(), "bad".to_string(), "new".to_string()]
        );
    }

    #[test]
    fn remembers_prompted_passwords_even_when_they_fail_current_backup() {
        let mut manager = PasswordManager::new();
        let mut prompts = vec![
            PasswordInput::Password("first".into()),
            PasswordInput::Password("second".into()),
            PasswordInput::Skip,
        ]
        .into_iter();

        let result = manager
            .unlock_with(
                |_password| -> Result<&str, ()> { Err(()) },
                || Ok(prompts.next().unwrap()),
                |_| true,
            )
            .unwrap();

        assert_eq!(result, PasswordResolution::Skip);
        assert_eq!(
            manager.known_passwords(),
            &["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn remembered_failed_passwords_are_tried_for_later_backups() {
        let mut manager = PasswordManager::new();
        let mut first_prompts = vec![
            PasswordInput::Password("candidate".into()),
            PasswordInput::Skip,
        ]
        .into_iter();
        let first = manager
            .unlock_with(
                |_password| -> Result<&str, ()> { Err(()) },
                || Ok(first_prompts.next().unwrap()),
                |_| true,
            )
            .unwrap();
        assert_eq!(first, PasswordResolution::Skip);

        let mut attempts = Vec::new();
        let second = manager
            .unlock_with(
                |password| {
                    attempts.push(password.to_string());
                    (password == "candidate").then_some("ok").ok_or(())
                },
                || panic!("remembered password should be tried before prompting"),
                |_| true,
            )
            .unwrap();

        assert_eq!(second, PasswordResolution::Unlocked("ok"));
        assert_eq!(attempts, vec!["candidate".to_string()]);
    }

    #[test]
    fn skip_stops_prompt_loop_without_remembering_password() {
        let mut manager = PasswordManager::new();
        manager.remember("old".into());
        let result = manager
            .unlock_with(
                |_password| -> Result<&str, ()> { Err(()) },
                || Ok(PasswordInput::Skip),
                |_| true,
            )
            .unwrap();

        assert_eq!(result, PasswordResolution::Skip);
        assert_eq!(manager.known_passwords(), &["old".to_string()]);
    }

    #[test]
    fn fatal_unlock_error_stops_without_prompting() {
        let mut manager = PasswordManager::new();
        manager.remember("old".into());

        let result = manager.unlock_with(
            |_password| -> Result<&str, &'static str> { Err("fatal") },
            || panic!("prompt should not be used after fatal known-password error"),
            |error| *error == "retry",
        );

        assert_eq!(result, Err("fatal"));
    }
}
