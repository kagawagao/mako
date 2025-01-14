use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::Value;
use swc_core::common::collections::AHashMap;
use swc_core::common::sync::Lrc;
use swc_core::common::{Mark, DUMMY_SP};
use swc_core::ecma::ast::{
    ArrayLit, Bool, ComputedPropName, Expr, ExprOrSpread, Ident, KeyValueProp, Lit, MemberExpr,
    MemberProp, MetaPropExpr, MetaPropKind, ModuleItem, Null, Number, ObjectLit, Prop, PropName,
    PropOrSpread, Stmt, Str,
};
use swc_core::ecma::atoms::{js_word, JsWord};
use swc_core::ecma::utils::{quote_ident, ExprExt};
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

use crate::ast::js_ast::JsAst;
use crate::compiler::Context;
use crate::config::ConfigError;

enum EnvsType {
    Node(Lrc<AHashMap<JsWord, Expr>>),
    Browser(Lrc<AHashMap<String, Expr>>),
}

#[derive(Debug)]
pub struct EnvReplacer {
    unresolved_mark: Mark,
    envs: Lrc<AHashMap<JsWord, Expr>>,
    meta_envs: Lrc<AHashMap<String, Expr>>,
}

impl EnvReplacer {
    pub fn new(envs: Lrc<AHashMap<JsWord, Expr>>, unresolved_mark: Mark) -> Self {
        let mut meta_env_map = AHashMap::default();

        // generate meta_envs from envs
        for (k, v) in envs.iter() {
            // convert NODE_ENV to MODE
            let key: String = if k.eq(&js_word!("NODE_ENV")) {
                "MODE".into()
            } else {
                k.to_string()
            };

            meta_env_map.insert(key, v.clone());
        }

        Self {
            unresolved_mark,
            envs,
            meta_envs: Lrc::new(meta_env_map),
        }
    }

    fn get_env(envs: &EnvsType, sym: &JsWord) -> Option<Expr> {
        match envs {
            EnvsType::Node(envs) => envs.get(sym).cloned(),
            EnvsType::Browser(envs) => envs.get(&sym.to_string()).cloned(),
        }
    }
}
impl VisitMut for EnvReplacer {
    fn visit_mut_expr(&mut self, expr: &mut Expr) {
        if let Expr::Ident(Ident { ref sym, span, .. }) = expr {
            let envs = EnvsType::Node(self.envs.clone());

            // 先判断 env 中的变量名称，是否是上下文中已经存在的变量名称
            if span.ctxt.outer() != self.unresolved_mark {
                expr.visit_mut_children_with(self);
                return;
            }

            if let Some(env) = EnvReplacer::get_env(&envs, sym) {
                // replace with real value if env found
                *expr = env;
                return;
            }
        }

        if let Expr::Member(MemberExpr { obj, prop, .. }) = expr {
            if let Expr::Member(MemberExpr {
                obj: first_obj,
                prop:
                    MemberProp::Ident(Ident {
                        sym: js_word!("env"),
                        ..
                    }),
                ..
            }) = &**obj
            {
                // handle `env.XX`
                let mut envs = EnvsType::Node(self.envs.clone());

                if match &**first_obj {
                    Expr::Ident(Ident {
                        sym: js_word!("process"),
                        ..
                    }) => true,
                    Expr::MetaProp(MetaPropExpr {
                        kind: MetaPropKind::ImportMeta,
                        ..
                    }) => {
                        envs = EnvsType::Browser(self.meta_envs.clone());
                        true
                    }
                    _ => false,
                } {
                    // handle `process.env.XX` and `import.meta.env.XX`
                    match prop {
                        MemberProp::Computed(ComputedPropName { expr: c, .. }) => {
                            if let Expr::Lit(Lit::Str(Str { value: sym, .. })) = &**c {
                                if let Some(env) = EnvReplacer::get_env(&envs, sym) {
                                    // replace with real value if env found
                                    *expr = env;
                                } else {
                                    // replace with `undefined` if env not found
                                    *expr = *Box::new(Expr::Ident(Ident::new(
                                        js_word!("undefined"),
                                        DUMMY_SP,
                                    )));
                                }
                            }
                        }

                        MemberProp::Ident(Ident { sym, .. }) => {
                            if let Some(env) = EnvReplacer::get_env(&envs, sym) {
                                // replace with real value if env found
                                *expr = env;
                            } else {
                                // replace with `undefined` if env not found
                                *expr = *Box::new(Expr::Ident(Ident::new(
                                    js_word!("undefined"),
                                    DUMMY_SP,
                                )));
                            }
                        }
                        _ => {}
                    }
                }
            } else if let Expr::Member(MemberExpr {
                obj:
                    box Expr::MetaProp(MetaPropExpr {
                        kind: MetaPropKind::ImportMeta,
                        ..
                    }),
                prop:
                    MemberProp::Ident(Ident {
                        sym: js_word!("env"),
                        ..
                    }),
                ..
            }) = *expr
            {
                // replace independent `import.meta.env` to json object
                let mut props = Vec::new();

                // convert envs to object properties
                for (k, v) in self.meta_envs.iter() {
                    props.push(PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
                        key: PropName::Ident(Ident::new(k.clone().into(), DUMMY_SP)),
                        value: Box::new(v.clone()),
                    }))));
                }

                *expr = Expr::Object(ObjectLit {
                    span: DUMMY_SP,
                    props,
                });
            }
        }

        expr.visit_mut_children_with(self);
    }
}

pub fn build_env_map(
    env_map: HashMap<String, Value>,
    context: &Arc<Context>,
) -> Result<AHashMap<JsWord, Expr>> {
    let mut map = AHashMap::default();
    for (k, v) in env_map.into_iter() {
        let expr = get_env_expr(v, context)?;
        map.insert(k.into(), expr);
    }
    Ok(map)
}

fn get_env_expr(v: Value, context: &Arc<Context>) -> Result<Expr> {
    match v {
        Value::String(v) => {
            // the string content is treat as expression, so it has to be parsed
            let ast = JsAst::build("_mako_internal/_define_.js", &v, context.clone()).unwrap();
            let module = ast.ast.body.first().unwrap();

            match module {
                ModuleItem::Stmt(Stmt::Expr(stmt_expr)) => {
                    return Ok(stmt_expr.expr.as_expr().clone());
                }
                _ => Err(anyhow!(ConfigError::InvalidateDefineConfig(v))),
            }
        }
        Value::Bool(v) => Ok(Bool {
            span: DUMMY_SP,
            value: v,
        }
        .into()),
        Value::Number(v) => Ok(Number {
            span: DUMMY_SP,
            raw: None,
            value: v.as_f64().unwrap(),
        }
        .into()),
        Value::Array(val) => {
            let mut elems = vec![];
            for item in val.iter() {
                elems.push(Some(ExprOrSpread {
                    spread: None,
                    expr: get_env_expr(item.clone(), context)?.into(),
                }));
            }

            Ok(ArrayLit {
                span: DUMMY_SP,
                elems,
            }
            .into())
        }
        Value::Null => Ok(Null { span: DUMMY_SP }.into()),
        Value::Object(val) => {
            let mut props = vec![];
            for (key, value) in val.iter() {
                let prop = PropOrSpread::Prop(
                    Prop::KeyValue(KeyValueProp {
                        key: quote_ident!(key.clone()).into(),
                        value: get_env_expr(value.clone(), context)?.into(),
                    })
                    .into(),
                );
                props.push(prop);
            }
            Ok(ObjectLit {
                span: DUMMY_SP,
                props,
            }
            .into())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use maplit::hashmap;
    use serde_json::{json, Value};
    use swc_core::common::sync::Lrc;
    use swc_core::common::GLOBALS;
    use swc_core::ecma::visit::VisitMutWith;

    use super::{build_env_map, EnvReplacer};
    use crate::ast::tests::TestUtils;
    use crate::compiler::Context;

    #[should_panic = "define value 'for(;;)console.log()' is not an Expression"]
    #[test]
    fn test_wrong_define_value() {
        let context: Arc<Context> = Arc::new(Default::default());
        build_env_map(
            hashmap! {
                "wrong".to_string() => json!("for(;;)console.log()")
            },
            &context,
        )
        .unwrap();
    }

    #[should_panic = "define value 'for(;;)console.log()' is not an Expression"]
    #[test]
    fn test_nested_wrong_define_value() {
        let context: Arc<Context> = Arc::new(Default::default());
        build_env_map(
            hashmap! {
                "parent".to_string() =>
                json!({"wrong": "for(;;)console.log()" })
            },
            &context,
        )
        .unwrap();
    }

    #[test]
    fn test_boolean() {
        assert_eq!(
            run(
                r#"log(A)"#,
                hashmap! {
                    "A".to_string() => json!(true)
                }
            ),
            r#"log(true);"#
        );
    }

    #[test]
    fn test_number() {
        assert_eq!(
            run(
                r#"log(A)"#,
                hashmap! {
                    "A".to_string() => json!(1)
                }
            ),
            r#"log(1);"#
        );
    }

    #[test]
    fn test_string() {
        assert_eq!(
            run(
                r#"log(A)"#,
                hashmap! {
                    "A".to_string() => json!("\"foo\"")
                }
            ),
            r#"log("foo");"#
        );
    }

    #[test]
    fn test_array() {
        assert_eq!(
            run(
                r#"log(A)"#,
                hashmap! {
                    "A".to_string() => json!([1, true, "\"foo\""])
                }
            ),
            r#"
log([
    1,
    true,
    "foo"
]);
            "#
            .trim()
        );
    }

    #[test]
    fn test_undefined_env() {
        assert_eq!(
            run(
                r#"if (process.env.UNDEFINED_ENV === "true") {}"#,
                Default::default()
            ),
            r#"if (undefined === "true") {}"#
        );
    }

    fn run(js_code: &str, envs: HashMap<String, Value>) -> String {
        let mut test_utils = TestUtils::gen_js_ast(js_code);
        let envs = build_env_map(envs, &test_utils.context).unwrap();
        let ast = test_utils.ast.js_mut();
        GLOBALS.set(&test_utils.context.meta.script.globals, || {
            let mut visitor = EnvReplacer::new(Lrc::new(envs), ast.unresolved_mark);
            ast.ast.visit_mut_with(&mut visitor);
        });
        test_utils.js_ast_to_code()
    }
}
