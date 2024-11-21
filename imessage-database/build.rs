use std::{
    env,
    fs::{copy, exists},
    path::PathBuf,
};

use protobuf::descriptor::field_descriptor_proto::Type;
use protobuf::reflect::{EnumDescriptor, FieldDescriptor, MessageDescriptor};
use protobuf_codegen::{Codegen, Customize, CustomizeCallback};

fn main() {
    build_proto(
        "src/message_types/handwriting/handwriting.proto",
        "handwriting.rs",
        "src/message_types/handwriting/handwriting_proto.rs",
    );
    build_proto(
        "src/message_types/digital_touch/digital_touch.proto",
        "digital_touch.rs",
        "src/message_types/digital_touch/digital_touch_proto.rs",
    );
}

fn build_proto(input_proto: &str, generated_name: &str, output_rs: &str) {
    struct GenSerde;

    impl CustomizeCallback for GenSerde {
        fn message(&self, _message: &MessageDescriptor) -> Customize {
            // Add Serde attributes for messages
            Customize::default().before("#[derive(::serde::Serialize)]")
        }

        fn field(&self, field: &FieldDescriptor) -> Customize {
            if field.proto().type_() == Type::TYPE_ENUM {
                // Add custom attributes for enum fields
                Customize::default()
                    .before("#[serde(serialize_with = \"crate::serialize_enum_or_unknown\")]")
            } else if field.proto().type_() == Type::TYPE_MESSAGE {
                // Add custom attributes for message fields
                Customize::default()
                    .before("#[serde(serialize_with = \"crate::serialize_message_field\")]")
            } else {
                Customize::default()
            }
        }

        fn special_field(&self, _message: &MessageDescriptor, _field: &str) -> Customize {
            // Skip special fields during serialization
            Customize::default().before("#[serde(skip)]")
        }

        fn enumeration(&self, _enum_type: &EnumDescriptor) -> Customize {
            // Add Serde attributes for enums
            Customize::default().before("#[derive(::serde::Serialize)]")
        }
    }

    if !exists(output_rs).unwrap() {
        Codegen::new()
            .pure()
            .input(input_proto)
            .include(".")
            .cargo_out_dir("p")
            .customize_callback(GenSerde)
            .run_from_script();

        // Move generated file to correct location
        let mut generated = PathBuf::from(env::var("OUT_DIR").unwrap());
        generated.push("p");
        generated.push(generated_name);
        println!("{}", generated.to_str().unwrap());
        copy(generated, output_rs).unwrap();
    }
}
