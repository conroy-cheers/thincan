include!(concat!(env!("OUT_DIR"), "/generated_mods.rs"));

use crate::monster::{finish_monster_buffer, Color, Equipment, Monster, MonsterArgs, MonsterBuilder};
use flatbuffers::FlatBufferBuilder;

pub fn foo() {
    let mut fbb = FlatBufferBuilder::new();

    let monster = {
        let args = MonsterArgs {
            pos: None,
            mana: 12,
            hp: 13,
            name: Some(fbb.create_string("Jeff")),
            inventory: Some(fbb.create_vector(&[1u8, 2u8, 3u8])),
            color: Color::Blue,
            weapons: None,
            equipped_type: Equipment::NONE,
            equipped: None,
            path: None,
        };
        let monster = Monster::create(&mut fbb, &args);
        finish_monster_buffer(&mut fbb, monster);
        fbb.finished_data()
    };
}
