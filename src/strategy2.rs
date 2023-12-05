use log::info;
use num_bigint::BigUint;
use num_traits::cast::FromPrimitive;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time;
use vega_crypto::Transact;
use vega_protobufs::vega::{
    commands::v1::{
        input_data::Command, BatchMarketInstructions, OrderCancellation, OrderSubmission,
    },
    instrument::Product,
    order::{TimeInForce, Type},
    Market, Side,
};
use vega_protobufs::vega::{Asset, Position};

use crate::{binance_ws::RefPrice, vega_store2::VegaStore};

pub async fn start(
    mut w1: Transact,
    mut w2: Transact,
    default_trade_size: i64,
    market: String,
    store: Arc<Mutex<VegaStore>>,
    rp: Arc<Mutex<RefPrice>>,
    submission_rate: u64,
) {
    // just loop forever, waiting for user interupt
    info!(
        "starting with submission rate of {} seconds",
        submission_rate
    );

    info!("closing all positions");
    match w1
        .send(Command::BatchMarketInstructions(get_close_batch(
            market.clone(),
        )))
        .await
    {
        Ok(o) => info!("w1 close batch result: {:?}", o),
        Err(e) => info!("w1 close batch transaction error: {:?}", e),
    };

    match w2
        .send(Command::BatchMarketInstructions(get_close_batch(
            market.clone(),
        )))
        .await
    {
        Ok(o) => info!("w2 close batch result: {:?}", o),
        Err(e) => info!("w2 close batch transaction error: {:?}", e),
    };

    let mut interval = time::interval(Duration::from_secs(submission_rate));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                interval.reset();
                let extra_sleep = rand::random::<u64>() % 10;
                info!("adding extra sleep of {} seconds before starting", extra_sleep);
                // add some extra time here jsut to look a little bit less scripted
                time::sleep(Duration::from_secs(extra_sleep)).await;
                run_strategy(&mut w1, &mut w2, market.clone(), store.clone(), rp.clone(), default_trade_size).await;
            }
        }
    }
}

async fn run_strategy(
    w1: &mut Transact,
    w2: &mut Transact,
    market: String,
    store: Arc<Mutex<VegaStore>>,
    rp: Arc<Mutex<RefPrice>>,
    mut default_trade_size: i64,
) {
    info!("executing trading strategy...");
    let mkt = store.lock().unwrap().get_market();
    let asset = store.lock().unwrap().get_asset(get_asset(&mkt));

    info!(
        "updating quotes for {}",
        mkt.tradable_instrument
            .as_ref()
            .unwrap()
            .instrument
            .as_ref()
            .unwrap()
            .name
    );

    default_trade_size = ((rand::random::<u64>() % default_trade_size as u64) + 1) as i64;
    info!("selected trade size: {}", default_trade_size,);

    let d = Decimals::new(&mkt, &asset);

    let (best_bid, best_ask) = rp.lock().unwrap().get();
    let mid_price = (best_ask + best_bid) / 2.;
    info!(
        "new reference prices: bestBid({}), bestAsk({}), midPrice({})",
        best_bid, best_ask, mid_price,
    );

    let md = store.lock().unwrap().get_market_data();
    // let price = BigUint::from_f64(d.to_market_price_precision(mid_price)).unwrap();
    let md_bid = BigUint::parse_bytes(md.best_bid_price.as_bytes(), 10).unwrap();
    let md_ask = BigUint::parse_bytes(md.best_offer_price.as_bytes(), 10).unwrap();
    let md_mid_price = (md_ask.clone() + md_bid.clone()) / BigUint::from_i64(2).unwrap();
    info!(
        "new vega reference prices: bestBid({}), bestAsk({}), midPrice({})",
        md_bid.to_string(),
        md_ask.to_string(),
        md_mid_price,
    );

    if best_ask == 0. || best_bid == 0. {
        info!("reference price are not up to date yet");
        return;
    }

    let w1_position_size = match store.lock().unwrap().get_position(&*w1.public_key()) {
        Some(p) => p.open_volume,
        None => 0,
    };

    let w2_position_size = match store.lock().unwrap().get_position(&*w2.public_key()) {
        Some(p) => p.open_volume,
        None => 0,
    };

    info!("wallet 1 open volume: {}", w1_position_size);
    info!("wallet 2 open volume: {}", w2_position_size);

    let (w1_order_size, w2_order_size, is_market) =
        get_order_sizes(w1_position_size, w2_position_size, default_trade_size);

    info!("wallet 1 order size: {}", w1_order_size);
    info!("wallet 2 order size: {}", w2_order_size);
    info!("submitting market orders: {}", is_market);

    // let price_in_m_precision = BigUint::from_f64(d.to_market_price_precision(mid_price)).unwrap();

    // info!(
    //     "price in market decimal: {}",
    //     price_in_m_precision.to_string(),
    // );

    let batch_w1 = Command::BatchMarketInstructions(get_batch(
        market.clone(),
        md_mid_price.to_string(),
        w1_order_size,
        is_market,
    ));
    let batch_w2 = Command::BatchMarketInstructions(get_batch(
        market.clone(),
        md_mid_price.to_string(),
        w2_order_size,
        is_market,
    ));

    if w1_order_size > 0 {
        match w1.send(batch_w1).await {
            Ok(o) => info!("w1 result: {:?}", o),
            Err(e) => info!("w1 transaction error: {:?}", e),
        };

        match w2.send(batch_w2).await {
            Ok(o) => info!("w2 result: {:?}", o),
            Err(e) => info!("w2 transaction error: {:?}", e),
        };
    } else {
        match w2.send(batch_w2).await {
            Ok(o) => info!("w2 result: {:?}", o),
            Err(e) => info!("w2 transaction error: {:?}", e),
        };
        match w1.send(batch_w1).await {
            Ok(o) => info!("w1 result: {:?}", o),
            Err(e) => info!("w1 transaction error: {:?}", e),
        };
    }

    // let (open_volume, aep) =
    //     volume_and_average_entry_price(&d, &store.lock().unwrap().get_position());

    // let mut submissions = get_order_submission(&d, best_bid, Side::Buy, market.clone(), bid_volume);
    // submissions.append(&mut get_order_submission(
    //     &d,
    //     best_ask,
    //     Side::Sell,
    //     market.clone(),
    //     offer_volume,
    // ));

    // let batch = BatchMarketInstructions {
    //     cancellations: vec![OrderCancellation {
    //         market_id: market.clone(),
    //         order_id: "".to_string(),
    //     }],
    //     amendments: vec![],
    //     submissions,
    // };

    // info!("batch submission: {:?}", batch);
    // clt.send(batch).await.unwrap();
}

fn get_batch(
    market_id: String,
    price: String,
    mut size: i64,
    is_market: bool,
) -> BatchMarketInstructions {
    let mut side = Side::Buy;
    if size < 0 {
        side = Side::Sell;
        size = -size;
    }
    let (tif, typ, price) = match is_market {
        true => (TimeInForce::Ioc, Type::Market, "".to_string()),
        false => (TimeInForce::Gfn, Type::Limit, price),
    };

    return BatchMarketInstructions {
        cancellations: vec![OrderCancellation {
            order_id: "".to_string(),
            market_id: market_id.clone(),
        }],
        amendments: vec![],
        submissions: vec![OrderSubmission {
            expires_at: 0,
            market_id: market_id.clone(),
            pegged_order: None,
            price: price,
            size: size as u64,
            reference: "".to_string(),
            side: side.into(),
            time_in_force: tif.into(),
            r#type: typ.into(),
            reduce_only: false,
            post_only: false,
            iceberg_opts: None,
        }],
        stop_orders_cancellation: vec![],
        stop_orders_submission: vec![],
    };
}

fn get_close_batch(market_id: String) -> BatchMarketInstructions {
    return BatchMarketInstructions {
        cancellations: vec![OrderCancellation {
            order_id: "".to_string(),
            market_id: market_id.clone(),
        }],
        amendments: vec![],
        submissions: vec![],
        stop_orders_cancellation: vec![],
        stop_orders_submission: vec![],
    };
}

fn get_order_sizes(
    w1_position_size: i64,
    w2_position_size: i64,
    default_trade_size: i64,
) -> (i64, i64, bool) {
    match (w1_position_size, w2_position_size) {
        (0, 0) => return (-default_trade_size, default_trade_size, false),
        (0, v) => {
            if v > 0 {
                return (default_trade_size, -default_trade_size, false);
            }
            return (-default_trade_size, default_trade_size, false);
        }
        (v, 0) => {
            if v > 0 {
                return (-default_trade_size, default_trade_size, false);
            }
            return (default_trade_size, -default_trade_size, false);
        }
        (v1, v2) if v1 > 0 && v2 > 0 => {
            return (-default_trade_size, -default_trade_size, true);
        }
        (v1, v2) if v1 > 0 && v2 < 0 => return (-default_trade_size, default_trade_size, false),
        (v1, v2) if v1 < 0 && v2 < 0 => {
            return (default_trade_size, default_trade_size, true);
        }
        (v1, v2) if v1 < 0 && v2 > 0 => return (default_trade_size, -default_trade_size, false),
        _ => unreachable!("all case should be covered, bad bad bad"),
    }
}

// fn get_order_submission(
//     d: &Decimals,
//     ref_price: f64,
//     side: vega_wallet_client::commands::Side,
//     market_id: String,
//     target_volume: f64,
// ) -> Vec<vega_wallet_client::commands::OrderSubmission> {
//     use vega_wallet_client::commands::{OrderSubmission, OrderType, Side, TimeInForce};

//     let size = target_volume / 5. * ref_price;

//     fn price_buy(ref_price: f64, f: f64) -> f64 {
//         ref_price * (1f64 - (f * 0.002))
//     }

//     fn price_sell(ref_price: f64, f: f64) -> f64 {
//         ref_price * (1f64 + (f * 0.002))
//     }

//     let price_f: fn(f64, f64) -> f64 = match side {
//         Side::Buy => price_buy,
//         Side::Sell => price_sell,
//         _ => panic!("should never happen"),
//     };

//     let mut orders: Vec<OrderSubmission> = vec![];
//     for i in vec![1, 2, 3, 4, 5].into_iter() {
//         let p =
//             BigUint::from_f64(d.to_market_price_precision(price_f(ref_price, i as f64))).unwrap();

//         orders.push(OrderSubmission {
//             market_id: market_id.clone(),
//             price: p.to_string(),
//             size: d.to_market_position_precision(size) as u64,
//             side,
//             time_in_force: TimeInForce::Gtc,
//             expires_at: 0,
//             r#type: OrderType::Limit,
//             reference: "VEGA_RUST_MM_SIMPLE".to_string(),
//             pegged_order: None,
//         });
//     }

//     return orders;
// }

// fn get_pubkey_balance(
//     store: Arc<Mutex<VegaStore>>,
//     pubkey: String,
//     asset_id: String,
//     d: &Decimals,
// ) -> f64 {
//     d.from_asset_precision(store.lock().unwrap().get_accounts().iter().fold(
//         0f64,
//         |balance, acc| {
//             if acc.asset != asset_id || acc.owner != pubkey {
//                 balance
//             } else {
//                 balance + acc.balance.parse::<f64>().unwrap()
//             }
//         },
//     ))
// }

// // return vol, aep
// fn volume_and_average_entry_price(d: &Decimals, pos: &Option<Position>) -> (f64, f64) {
//     if let Some(p) = pos {
//         let vol = p.open_volume as f64;
//         let aep = p.average_entry_price.parse::<f64>().unwrap();
//         return (
//             d.from_market_position_precision(vol),
//             d.from_market_price_precision(aep),
//         );
//     }

//     return (0., 0.);
// }

fn get_asset(mkt: &Market) -> String {
    match mkt
        .clone()
        .tradable_instrument
        .unwrap()
        .instrument
        .unwrap()
        .product
        .unwrap()
    {
        Product::Future(f) => f.settlement_asset,
        Product::Spot(_) => unimplemented!("spot market not supported"),
        Product::Perpetual(f) => f.settlement_asset,
    }
}

struct Decimals {
    position_factor: f64,
    price_factor: f64,
    asset_factor: f64,
}

impl Decimals {
    fn new(mkt: &Market, asset: &Asset) -> Decimals {
        return Decimals {
            position_factor: (10_f64).powf(mkt.position_decimal_places as f64),
            price_factor: (10_f64).powf(mkt.decimal_places as f64),
            asset_factor: (10_f64).powf(asset.details.as_ref().unwrap().decimals as f64),
        };
    }

    fn from_asset_precision(&self, amount: f64) -> f64 {
        return amount / self.asset_factor;
    }

    fn from_market_price_precision(&self, price: f64) -> f64 {
        return price / self.price_factor;
    }

    fn from_market_position_precision(&self, position: f64) -> f64 {
        return position / self.position_factor;
    }

    fn to_market_price_precision(&self, price: f64) -> f64 {
        return price * self.price_factor;
    }

    fn to_market_position_precision(&self, position: f64) -> f64 {
        return position * self.position_factor;
    }
}

// async fn run_strategy(
//     clt: &WalletClient,
//     pubkey: String,
//     market: String,
//     store: Arc<Mutex<VegaStore>>,
//     rp: Arc<Mutex<RefPrice>>,
// ) {
//     info!("executing trading strategy...");
//     let mkt = store.lock().unwrap().get_market();
//     let asset = store.lock().unwrap().get_asset(get_asset(&mkt));

//     info!(
//         "updating quotes for {}",
//         mkt.tradable_instrument
//             .as_ref()
//             .unwrap()
//             .instrument
//             .as_ref()
//             .unwrap()
//             .name
//     );

//     let d = Decimals::new(&mkt, &asset);

//     let (best_bid, best_ask) = rp.lock().unwrap().get();
//     info!(
//         "new reference prices: bestBid({}), bestAsk({})",
//         best_bid, best_ask
//     );

//     let (open_volume, aep) =
//         volume_and_average_entry_price(&d, &store.lock().unwrap().get_position());

//     let balance = get_pubkey_balance(store.clone(), pubkey.clone(), asset.id.clone(), &d);
//     info!("pubkey balance: {}", balance);

//     let bid_volume = balance * 0.5 - open_volume * aep;
//     let offer_volume = balance * 0.5 + open_volume * aep;
//     let notional_exposure = (open_volume * aep).abs();
//     info!(
//         "openvolume({}), entryPrice({}), notionalExposure({})",
//         open_volume, aep, notional_exposure,
//     );
//     info!("bidVolume({}), offerVolume({})", bid_volume, offer_volume);

//     use vega_wallet_client::commands::{BatchMarketInstructions, OrderCancellation, Side};

//     let mut submissions = get_order_submission(&d, best_bid, Side::Buy, market.clone(), bid_volume);
//     submissions.append(&mut get_order_submission(
//         &d,
//         best_ask,
//         Side::Sell,
//         market.clone(),
//         offer_volume,
//     ));
//     let batch = BatchMarketInstructions {
//         cancellations: vec![OrderCancellation {
//             market_id: market.clone(),
//             order_id: "".to_string(),
//         }],
//         amendments: vec![],
//         submissions,
//     };

//     info!("batch submission: {:?}", batch);
//     clt.send(batch).await.unwrap();
// }
