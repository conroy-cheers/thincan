capnp::generated_code!(pub mod person_capnp);

#[cfg(test)]
mod tests {
    use capnp::message::{ReaderOptions, SingleSegmentAllocator};

    use super::person_capnp::person;

    #[test]
    fn test_person() {
        let mut buf = [0u8; 64];

        let mut person_builder =
            capnp::message::Builder::new(SingleSegmentAllocator::new(&mut buf));
        let mut person: person::Builder = person_builder.init_root();

        person.set_email("bob@example.com");
        person.set_name("Bob Jones");

        assert_eq!(person.get_email().unwrap(), "bob@example.com");

        let mut test_buf = [0u8; 64];
        capnp::serialize::write_message(&mut test_buf[..], &person_builder).unwrap();

        let mut out_buf = [0u8; 64];
        let read_output = capnp::serialize::read_message_no_alloc(
            &test_buf[..],
            &mut out_buf,
            ReaderOptions::default(),
        )
        .unwrap();

        let reader = read_output.get_root::<person::Reader>().unwrap();
        assert_eq!(reader.get_email().unwrap(), "bob@example.com");
        assert_eq!(reader.get_name().unwrap(), "Bob Jones");
    }
}
