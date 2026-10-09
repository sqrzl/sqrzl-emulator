use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

type Gate = tokio::sync::Mutex<()>;
static GATES: OnceLock<Mutex<HashMap<usize, Weak<Gate>>>> = OnceLock::new();

pub(super) fn operation_gate<T: ?Sized>(storage: &T) -> Arc<Gate> {
    let identity = std::ptr::from_ref(storage).cast::<()>() as usize;
    let mut gates = GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    gates.retain(|_, gate| gate.strong_count() > 0);
    if let Some(gate) = gates.get(&identity).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(Gate::new(()));
    gates.insert(identity, Arc::downgrade(&gate));
    gate
}
