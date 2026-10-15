//! The service's `SmallModels` (R2, `docs/r2-brief.md` D1, D2): a provider's
//! small-tasks model resolved through `AppState::model_for(Route::Small, provider)` and
//! `Generator::text_model`. The registry is built before the `App` it reads from, so
//! the state is bound after (`bind`). R2-retrieve builds this file and passes it as
//! `StageDeps.models` in `server.rs`.

use std::sync::{OnceLock, Weak};

use margin_shared::Provider;

use crate::engines::BoxFuture;
use crate::pipeline::{SmallModel, SmallModels};
use crate::state::App;

/// `SmallModels` over the running service's settings and generator.
#[derive(Default)]
pub struct StateModels {
    state: OnceLock<Weak<App>>,
}

impl StateModels {
    /// Binds the service once its `App` exists.
    pub fn bind(&self, state: Weak<App>) {
        let _ = state;
        unimplemented!("R2-retrieve, StateModels::bind")
    }
}

impl SmallModels for StateModels {
    fn small<'a>(
        &'a self,
        provider: Option<Provider>,
    ) -> BoxFuture<'a, anyhow::Result<SmallModel>> {
        let _ = (&self.state, provider);
        unimplemented!("R2-retrieve, StateModels::small")
    }
}
