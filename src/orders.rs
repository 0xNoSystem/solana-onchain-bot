use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum PositionOp {
    Open,
    Close,
}

#[derive(Copy, Clone, Debug)]
pub struct EngineOrder {
    pub action: PositionOp,
    // For `Open`, this is base notional (SOL). For `Close`, this remains asset size.
    pub size: f64,
}

impl EngineOrder {
    pub fn new_market(action: PositionOp, size: f64) -> Self {
        EngineOrder { action, size }
    }

    pub fn market_open(size: f64) -> Self {
        Self::new_market(PositionOp::Open, size)
    }

    pub fn market_close(size: f64) -> Self {
        Self::new_market(PositionOp::Close, size)
    }
}

#[derive(Clone, Debug)]
pub enum ExecCommand {
    Order(EngineOrder),
    Control(ExecControl),
}

#[derive(Clone, Copy, Debug)]
pub enum ExecControl {
    Kill,
    Pause,
    Resume,
    ForceClose,
}
