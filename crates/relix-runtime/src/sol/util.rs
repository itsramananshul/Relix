use crate::sol::parser::Type;

pub enum TypeMismatch {
    ArraySize,
    Inequal,
}
pub fn type_eq(lhs: Type, rhs: Type) -> Result<(), TypeMismatch> {
    match lhs {
        Type::Ident(vlhs) => {
            if let Type::Ident(vrhs) = rhs
                && vlhs == vrhs
            {
                Ok(())
            } else {
                Err(TypeMismatch::Inequal)
            }
        }
        Type::Array {
            size: size_lhs,
            inner: inner_lhs,
        } => {
            if let Type::Array {
                size: size_rhs,
                inner: inner_rhs,
            } = rhs
                && type_eq(*inner_lhs, *inner_rhs).is_ok()
            {
                if size_lhs == size_rhs {
                    Ok(())
                } else {
                    Err(TypeMismatch::ArraySize)
                }
            } else {
                Err(TypeMismatch::Inequal)
            }
        }
        Type::Tuple(types_lhs) => {
            if let Type::Tuple(types_rhs) = rhs {
                if types_lhs
                    .iter()
                    .zip(types_rhs)
                    .any(|(l, r)| type_eq(l.to_owned(), r).is_err())
                {
                    Err(TypeMismatch::Inequal)
                } else {
                    Ok(())
                }
            } else {
                Err(TypeMismatch::Inequal)
            }
        }
        Type::Function {
            params: params_lhs,
            ret: ret_lhs,
        } => {
            if let Type::Function {
                params: params_rhs,
                ret: ret_rhs,
            } = rhs
            {
                match (
                    type_eq(*ret_lhs, Type::Void).is_ok(),
                    type_eq(*ret_rhs, Type::Void).is_ok(),
                ) {
                    (true, false) | (false, true) => Err(TypeMismatch::Inequal),
                    _ => {
                        if params_lhs
                            .iter()
                            .zip(params_rhs)
                            .any(|(l, r)| type_eq(l.to_owned(), r).is_err())
                        {
                            Err(TypeMismatch::Inequal)
                        } else {
                            Ok(())
                        }
                    }
                }
            } else {
                Err(TypeMismatch::Inequal)
            }
        }
        Type::Integer if matches!(rhs, Type::Integer) => Ok(()),
        Type::Float if matches!(rhs, Type::Float) => Ok(()),
        Type::String if matches!(rhs, Type::String) => Ok(()),
        Type::Char if matches!(rhs, Type::Char) => Ok(()),
        Type::Bool if matches!(rhs, Type::Bool) => Ok(()),
        Type::Void if matches!(rhs, Type::Void) => Ok(()),
        // F5 / F7: list and map are scalar nominal types in
        // the analyzer's eyes — there's no inner-type
        // parameterisation (yet) because the VM treats
        // elements / values as raw heap refs.
        Type::List if matches!(rhs, Type::List) => Ok(()),
        Type::Map if matches!(rhs, Type::Map) => Ok(()),
        _ => Err(TypeMismatch::Inequal),
    }
}

// pub fn type_size(t: Type) -> usize {
//     match t {
//         Type::Void => 0,
//         Type::Function { .. } => 0,
//         Type::Char => 1,
//         Type::Bool => 1,
//         Type::Integer => 8,
//         Type::Float => 8,
//         Type::String => 8,
//         Type::Array { .. } => 8,
//         Type::Tuple(subs) => subs.iter().map(|x| type_size(x.clone())).sum(),
//         Type::Ident(_) => 8,
//     }
// }
