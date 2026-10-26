fn main() {
    let caps = meshlive::bluetooth::probe();
    println!(
        "{}; LE={:?}; central={:?}; peripheral={:?}; extended={:?}; max_adv={:?}; error={:?}",
        caps.status(),
        caps.low_energy,
        caps.central,
        caps.peripheral,
        caps.extended_advertising,
        caps.max_advertisement_bytes,
        caps.error
    );
}
