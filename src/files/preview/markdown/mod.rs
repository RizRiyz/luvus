mod parser;

pub fn parse(source: &str) -> Vec<super::Block> {
    parser::parse(source)
}
