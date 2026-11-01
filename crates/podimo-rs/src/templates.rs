//! minijinja environment. Loads templates either from disk (for dev) or
//! embedded at compile time (for the release binary, so the container needs no
//! external state).

use std::sync::Arc;

use minijinja::Environment;

#[derive(Clone)]
pub(crate) struct Templates {
    env: Arc<Environment<'static>>,
}

impl Templates {
    pub(crate) fn new() -> Self {
        let mut env = Environment::new();
        env.add_template("base.html", include_str!("../templates/base.html"))
            .expect("base.html embedded");
        env.add_template("index.html", include_str!("../templates/index.html"))
            .expect("index.html embedded");
        env.add_template(
            "feed_location.html",
            include_str!("../templates/feed_location.html"),
        )
        .expect("feed_location.html embedded");
        env.add_template("library.html", include_str!("../templates/library.html"))
            .expect("library.html embedded");
        env.add_template("setup.html", include_str!("../templates/setup.html"))
            .expect("setup.html embedded");
        Self { env: Arc::new(env) }
    }

    pub(crate) fn render<S: serde::Serialize>(
        &self,
        name: &str,
        ctx: &S,
    ) -> Result<String, minijinja::Error> {
        let tmpl = self.env.get_template(name)?;
        tmpl.render(minijinja::value::Serde(ctx))
    }
}

impl Default for Templates {
    fn default() -> Self {
        Self::new()
    }
}
