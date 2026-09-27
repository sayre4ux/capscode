//! Container runtime adapters for Docker, OrbStack, and Apple Container.

mod apple;
mod engine;
mod selection;

pub use apple::AppleContainerAdapter;
pub use engine::{cancellable_stdin, is_capscode_image, DockerAdapter, OrbStackAdapter};
pub use selection::detect;
