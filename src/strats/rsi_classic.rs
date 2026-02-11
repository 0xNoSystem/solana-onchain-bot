#![allow(unused_variables)]
use super::*;
use TimeFrame::*;
use Value::*;

const RSI_BUY: f64 = 30.0;

pub struct RsiClassic {
    rsi_1m: IndexId,
}

impl RsiClassic {
    pub fn init() -> Self {
        let inds = Self::required_indicators_static();
        RsiClassic { rsi_1m: inds[0] }
    }
}

impl NeedsIndicators for RsiClassic {
    fn required_indicators_static() -> Vec<IndexId> {
        vec![(IndicatorKind::Rsi(12), TimeFrame::Min1)]
    }
}

impl Strat for RsiClassic {
    fn required_indicators(&self) -> Vec<IndexId> {
        Self::required_indicators_static()
    }

    fn on_idle(&mut self, ctx: StratContext, _armed: Armed) -> Option<Intent> {
        let rsi_value = match ctx.indicators.get(&self.rsi_1m)?.value {
            RsiValue(v) => v,
            _ => return None,
        };

        if rsi_value < RSI_BUY {
            return Some(Intent::open_market(SizeSpec::MarginPct(90.0)));
        }

        None
    }

    fn on_open(&mut self, ctx: StratContext, open_pos: &OpenPosInfo) -> Option<Intent> {
        let hold_ms = timedelta!(Min1, 5).as_ms();
        if ctx.last_price.open_time.saturating_sub(open_pos.open_time) >= hold_ms {
            return Some(Intent::flatten_market());
        }
        None
    }

    fn on_busy(&mut self, _ctx: StratContext, _busy: BusyType) -> Option<Intent> {
        None
    }
}
