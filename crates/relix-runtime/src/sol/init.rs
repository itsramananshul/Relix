use super::{
    analyzer::Analyzer,
    bytecode::Codegen,
    lexer::Lexer,
    parser::{Parser, Program},
    vm::VM,
};

pub fn init(scripts: Vec<&str>) -> Vec<VM> {
    scripts
        .into_iter()
        .map(|x| {
            let mut lexer = Lexer::from(x);
            let tokens = lexer.tokens();

            let mut parser = Parser::from(tokens);
            let mut program: Program = parser.run();

            let mut analyzer = Analyzer::new();
            analyzer.run(&mut program);

            let mut codegen = Codegen::from(analyzer.tt_arena);
            let bytecode = codegen.gen_bcode(&program);

            VM::from(&bytecode)
        })
        .collect()
}
