use embedded_can::ExtendedId;
use embedded_can::StandardId;
use embedded_can_interface::Id;

use can_iso_tp::{IsoTpAddress, IsoTpConfig, RxAddress, TargetAddressType, TxAddress};

fn std_id(raw: u16) -> Id {
    Id::Standard(StandardId::new(raw).unwrap())
}

fn ext_raw(id: Id) -> u32 {
    match id {
        Id::Extended(e) => e.as_raw(),
        other => panic!("expected extended id, got {other:?}"),
    }
}

#[test]
fn normal_address_maps_ids_and_no_addrs() {
    let tx_id = std_id(0x123);
    let rx_id = std_id(0x456);

    let addr = IsoTpAddress::normal(tx_id, rx_id);
    assert_eq!(addr.tx_id, tx_id);
    assert_eq!(addr.rx_id, rx_id);
    assert_eq!(addr.tx_addr, None);
    assert_eq!(addr.rx_addr, None);

    let cfg = IsoTpConfig::from(addr);
    assert_eq!(cfg.tx_id, tx_id);
    assert_eq!(cfg.rx_id, rx_id);
    assert_eq!(cfg.tx_addr, None);
    assert_eq!(cfg.rx_addr, None);
}

#[test]
fn extended_address_sets_prefix_bytes() {
    let tx_id = std_id(0x700);
    let rx_id = std_id(0x701);

    let addr = IsoTpAddress::extended(tx_id, rx_id, 0xAA, 0xBB);
    assert_eq!(addr.tx_id, tx_id);
    assert_eq!(addr.rx_id, rx_id);
    assert_eq!(addr.tx_addr, Some(0xBB));
    assert_eq!(addr.rx_addr, Some(0xAA));
}

#[test]
fn mixed_11_uses_same_extension_for_tx_and_rx() {
    let tx_id = std_id(0x700);
    let rx_id = std_id(0x701);

    let addr = IsoTpAddress::mixed_11(tx_id, rx_id, 0xCC);
    assert_eq!(addr.tx_id, tx_id);
    assert_eq!(addr.rx_id, rx_id);
    assert_eq!(addr.tx_addr, Some(0xCC));
    assert_eq!(addr.rx_addr, Some(0xCC));
}

#[test]
fn normal_fixed_29_physical_and_functional_ids_are_computed() {
    let src = 0xAA;
    let dst = 0xBB;

    let phys = IsoTpAddress::normal_fixed_29(src, dst, TargetAddressType::Physical).unwrap();
    assert_eq!(phys.tx_addr, None);
    assert_eq!(phys.rx_addr, None);
    assert_eq!(
        ext_raw(phys.tx_id),
        0x18DA0000 | ((dst as u32) << 8) | (src as u32)
    );
    assert_eq!(
        ext_raw(phys.rx_id),
        0x18DA0000 | ((src as u32) << 8) | (dst as u32)
    );

    let func = IsoTpAddress::normal_fixed_29(src, dst, TargetAddressType::Functional).unwrap();
    assert_eq!(
        ext_raw(func.tx_id),
        0x18DB0000 | ((dst as u32) << 8) | (src as u32)
    );
    assert_eq!(
        ext_raw(func.rx_id),
        0x18DB0000 | ((src as u32) << 8) | (dst as u32)
    );
}

#[test]
fn mixed_29_ids_and_extension_are_computed() {
    let src = 0x01;
    let dst = 0x02;
    let ext = 0xEE;

    let phys = IsoTpAddress::mixed_29(src, dst, ext, TargetAddressType::Physical).unwrap();
    assert_eq!(phys.tx_addr, Some(ext));
    assert_eq!(phys.rx_addr, Some(ext));
    assert_eq!(
        ext_raw(phys.tx_id),
        0x18CE0000 | ((dst as u32) << 8) | (src as u32)
    );
    assert_eq!(
        ext_raw(phys.rx_id),
        0x18CE0000 | ((src as u32) << 8) | (dst as u32)
    );

    let func = IsoTpAddress::mixed_29(src, dst, ext, TargetAddressType::Functional).unwrap();
    assert_eq!(
        ext_raw(func.tx_id),
        0x18CD0000 | ((dst as u32) << 8) | (src as u32)
    );
    assert_eq!(
        ext_raw(func.rx_id),
        0x18CD0000 | ((src as u32) << 8) | (dst as u32)
    );
}

#[test]
fn tx_and_rx_address_helpers_are_constructible() {
    let std = std_id(0x123);
    let ext = Id::Extended(ExtendedId::new(0x1ABCDE0).unwrap());

    let tx = TxAddress::normal(std);
    assert_eq!(tx.id, std);
    assert_eq!(tx.addr, None);

    let tx_ext = TxAddress::extended(ext, 0x11);
    assert_eq!(tx_ext.id, ext);
    assert_eq!(tx_ext.addr, Some(0x11));

    let rx = RxAddress::normal(std);
    assert_eq!(rx.id, std);
    assert_eq!(rx.addr, None);

    let rx_ext = RxAddress::extended(ext, 0x22);
    assert_eq!(rx_ext.id, ext);
    assert_eq!(rx_ext.addr, Some(0x22));
}
