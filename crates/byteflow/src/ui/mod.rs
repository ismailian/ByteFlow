//! Presentation layer. Talks only to `bf-app` (never to `bf-data` directly
//! for anything but plain data types), per spec §2.1.

pub mod encryption;
pub mod files;
pub mod icons;
pub mod links;
pub mod onboarding;
pub mod settings;
pub mod shell;
pub mod sync;
pub mod thumbs;
pub mod transfers;
pub mod unlock;
pub mod viewer;
pub mod widgets;
