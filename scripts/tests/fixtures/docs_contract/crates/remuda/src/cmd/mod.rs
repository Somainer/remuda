//! Minimal clap surface for the docs-contract fixture.

macro_rules! commands {
    ($($variant:ident($module:ident::$args:ident)),* $(,)?) => {};
}

// Declare a module above, then register its parser and dispatch once below.
commands! {
    Demo(demo::Args),
}
