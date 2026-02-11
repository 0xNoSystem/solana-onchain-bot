#![allow(unused_variables)]
use super::*;
use TimeFrame::*;
use Value::*;

pub struct SrsiAdxScalp {
    rsi_1h: IndexId,
    sma_rsi_1h: IndexId,
    adx_15m: IndexId,
    prev_rsi_above_sma: Option<bool>,
}

impl SrsiAdxScalp {
    pub fn init() -> Self {
        let inds = Self::required_indicators_static();
        Self {
            rsi_1h: inds[0],
            sma_rsi_1h: inds[1],
            adx_15m: inds[2],
            prev_rsi_above_sma: None,
        }
    }
}

impl NeedsIndicators for SrsiAdxScalp {
    fn required_indicators_static() -> Vec<IndexId> {
        vec![
            (IndicatorKind::Rsi(14), Hour1),
            (
                IndicatorKind::SmaOnRsi {
                    periods: 14,
                    smoothing_length: 10,
                },
                Hour1,
            ),
            (
                IndicatorKind::Adx {
                    periods: 10,
                    di_length: 10,
                },
                Min15,
            ),
        ]
    }
}

impl Strat for SrsiAdxScalp {
    fn required_indicators(&self) -> Vec<IndexId> {
        Self::required_indicators_static()
    }

    // =========================
    // IDLE: setup + trigger
    // =========================
    fn on_idle(&mut self, ctx: StratContext, armed: Armed) -> Option<Intent> {
        let StratContext {
            free_margin,
            last_price,
            indicators,
        } = ctx;

        // ADX logic only evaluated on ADX close when flat (same as vanilla)
        let adx_val = indicators.get(&self.adx_15m)?;
        if !adx_val.on_close {
            return None;
        }

        let rsi_1h = match indicators.get(&self.rsi_1h)?.value {
            RsiValue(v) => v,
            _ => return None,
        };

        let sma_rsi_1h = match indicators.get(&self.sma_rsi_1h)?.value {
            SmaRsiValue(v) => v,
            _ => return None,
        };

        let adx_15m = match adx_val.value {
            AdxValue(v) => v,
            _ => return None,
        };

        let rsi_above_sma = rsi_1h > sma_rsi_1h;

        // -------------------------
        // Trigger while armed
        // -------------------------
        if armed.is_some() {
            if let Some(prev) = self.prev_rsi_above_sma
                && !prev && rsi_above_sma {
                    let size = SizeSpec::MarginPct(95.0);
                    return Some(Intent::open_market(size));
            }
        } else {
            // -------------------------
            // Arm setup window
            // -------------------------
            if adx_15m > 48.0 && !rsi_above_sma {
                return Some(Intent::Arm(timedelta!(Hour1, 3)));
            }
        }

        self.prev_rsi_above_sma = Some(rsi_above_sma);
        None
    }

    // =========================
    // OPEN: position management
    // =========================
    fn on_open(&mut self, ctx: StratContext, open_pos: &OpenPosInfo) -> Option<Intent> {
        let StratContext {
            last_price,
            indicators,
            ..
        } = ctx;

        let rsi_1h = match indicators.get(&self.rsi_1h)?.value {
            RsiValue(v) => v,
            _ => return None,
        };

        let sma_rsi_1h = match indicators.get(&self.sma_rsi_1h)?.value {
            SmaRsiValue(v) => v,
            _ => return None,
        };

        // -------- Exit logic --------
        if rsi_1h < 60.0 {
            return Some(Intent::flatten_market());
        }

        if rsi_1h > 60.0 && (rsi_1h - sma_rsi_1h) < (rsi_1h * 0.1) {
            return Some(Intent::flatten_market());
        }

        None
    }

    // =========================
    // BUSY: no abort logic
    // =========================
    fn on_busy(&mut self, _ctx: StratContext, _busy: BusyType) -> Option<Intent> {
        None
    }
}
