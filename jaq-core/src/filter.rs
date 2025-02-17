use crate::box_iter::{self, box_once, flat_map_then, flat_map_then_with, flat_map_with, map_with};
use crate::compile::{Fold, Lut, Pattern, Tailrec, Term as Ast};
use crate::fold::fold;
use crate::val::{ValT, ValX, ValXs};
use crate::{exn, rc_lazy_list, Bind, Bind2, Ctx, Error, Exn};
use alloc::boxed::Box;
use dyn_clone::DynClone;

pub(crate) use crate::compile::TermId as Id;

// we can unfortunately not make a `Box<dyn ... + Clone>`
// that is why we have to go through the pain of making a new trait here
pub trait Update<'a, V>: Fn(V) -> ValXs<'a, V> + DynClone {}

impl<'a, V, T: Fn(V) -> ValXs<'a, V> + Clone> Update<'a, V> for T {}

dyn_clone::clone_trait_object!(<'a, V> Update<'a, V>);

type BoxUpdate<'a, V> = Box<dyn Update<'a, V> + 'a>;

type Results<'a, T, V> = crate::box_iter::Results<'a, T, Exn<'a, V>>;

/// Enhance the context `ctx` with variables bound to the outputs of `args` executed on `cv`,
/// and return the enhanced contexts together with the original value of `cv`.
///
/// This is used when we call filters with variable arguments.
fn bind_vars<'a, F: FilterT>(
    args: &'a [Bind<Id>],
    lut: &'a Lut<F>,
    ctx: Ctx<'a, F::V>,
    cv: Cv<'a, F::V>,
) -> Results<'a, Cv<'a, F::V>, F::V> {
    match args.split_first() {
        Some((Bind::Var(arg), [])) => {
            map_with(arg.run(lut, cv.clone()), (ctx, cv.1), |y, (ctx, v)| {
                Ok((ctx.cons_var(y?), v))
            })
        }
        Some((Bind::Fun(arg), [])) => box_once(Ok((ctx.cons_fun((arg, cv.0)), cv.1))),
        Some((Bind::Var(arg), rest)) => {
            flat_map_then_with(arg.run(lut, cv.clone()), (ctx, cv), |y, (ctx, cv)| {
                bind_vars(rest, lut, ctx.cons_var(y), cv)
            })
        }
        Some((Bind::Fun(arg), rest)) => bind_vars(rest, lut, ctx.cons_fun((arg, cv.0.clone())), cv),
        None => box_once(Ok((ctx, cv.1))),
    }
}

fn bind_pat<'a, F: FilterT>(
    (idxs, pat): &'a (Id, Pattern<Id>),
    lut: &'a Lut<F>,
    ctx: Ctx<'a, F::V>,
    cv: Cv<'a, F::V>,
) -> Results<'a, Ctx<'a, F::V>, F::V> {
    let (ctx0, v0) = cv.clone();
    let v1 = map_with(idxs.run(lut, cv), v0, move |i, v0| Ok(v0.index(&i?)?));
    match pat {
        Pattern::Var => Box::new(v1.map(move |v| Ok(ctx.clone().cons_var(v?)))),
        Pattern::Idx(pats) => flat_map_then_with(v1, (ctx, ctx0), move |v, (ctx, ctx0)| {
            bind_pats(pats, lut, ctx, (ctx0, v))
        }),
    }
}

fn bind_pats<'a, F: FilterT>(
    pats: &'a [(Id, Pattern<Id>)],
    lut: &'a Lut<F>,
    ctx: Ctx<'a, F::V>,
    cv: Cv<'a, F::V>,
) -> Results<'a, Ctx<'a, F::V>, F::V> {
    match pats.split_first() {
        None => box_once(Ok(ctx)),
        Some((pat, [])) => bind_pat(pat, lut, ctx, cv),
        Some((pat, rest)) => {
            flat_map_then_with(bind_pat(pat, lut, ctx, cv.clone()), cv, |ctx, cv| {
                bind_pats(rest, lut, ctx, cv)
            })
        }
    }
}

fn run_and_bind<'a, F: FilterT>(
    xs: &'a Id,
    lut: &'a Lut<F>,
    cv: Cv<'a, F::V>,
    pat: &'a Pattern<Id>,
) -> Results<'a, Ctx<'a, F::V>, F::V> {
    let xs = xs.run(lut, (cv.0.clone(), cv.1));
    match pat {
        Pattern::Var => map_with(xs, cv.0, move |y, ctx| Ok(ctx.cons_var(y?))),
        Pattern::Idx(pats) => flat_map_then_with(xs, cv.0, |y, ctx| {
            bind_pats(pats, lut, ctx.clone(), (ctx, y))
        }),
    }
}

fn reduce<'a, T, V, F>(xs: Results<'a, T, V>, init: V, f: F) -> ValXs<'a, V>
where
    T: Clone + 'a,
    V: Clone + 'a,
    F: Fn(T, V) -> ValXs<'a, V> + 'a,
{
    let xs = rc_lazy_list::List::from_iter(xs);
    Box::new(fold(xs, init, f, |_| (), |_, _| None, Some))
}

fn lazy<I: Iterator, F: FnOnce() -> I>(f: F) -> impl Iterator<Item = I::Item> {
    core::iter::once_with(f).flatten()
}

#[test]
fn lazy_is_lazy() {
    let f = || panic!();
    let mut iter = core::iter::once(0).chain(lazy(|| box_once(f())));
    assert_eq!(iter.size_hint(), (1, None));
    assert_eq!(iter.next(), Some(0));
}

/// Combination of context and input value.
pub type Cv<'c, V> = (Ctx<'c, V>, V);

/// A filter which is implemented using function pointers.
#[derive(Clone)]
pub struct Native<V> {
    run: RunPtr<V>,
    update: UpdatePtr<V>,
}

/// Run function pointer.
///
/// Implementation-wise, this would be a perfect spot for `for<'a, F: FilterT<V>>`;
/// unfortunately, this is [not stable yet](https://github.com/rust-lang/rust/issues/108185).
/// That would also allow to eliminate `F` from `FilterT`.
pub type RunPtr<V, F = Native<V>> = for<'a> fn(&'a Lut<F>, Cv<'a, V>) -> ValXs<'a, V>;
/// Update function pointer.
pub type UpdatePtr<V, F = Native<V>> =
    for<'a> fn(&'a Lut<F>, Cv<'a, V>, BoxUpdate<'a, V>) -> ValXs<'a, V>;

impl<V> Native<V> {
    /// Create a native filter from a run function, without support for updates.
    pub const fn new(run: RunPtr<V, Self>) -> Self {
        Self {
            run,
            update: |_, _, _| box_once(Err(Exn::from(Error::path_expr()))),
        }
    }

    /// Specify an update function (used for `filter |= ...`).
    pub const fn with_update(self, update: UpdatePtr<V, Self>) -> Self {
        Self { update, ..self }
    }
}

impl<V: ValT> FilterT for Native<V> {
    type V = V;

    fn run<'a>(&'a self, lut: &'a Lut<Self>, cv: Cv<'a, V>) -> ValXs<'a, V> {
        (self.run)(lut, cv)
    }

    fn update<'a>(
        &'a self,
        lut: &'a Lut<Self>,
        cv: Cv<'a, V>,
        f: BoxUpdate<'a, V>,
    ) -> ValXs<'a, V> {
        (self.update)(lut, cv, f)
    }
}

impl<F: FilterT<F>> FilterT<F> for Id {
    type V = F::V;

    fn run<'a>(&'a self, lut: &'a Lut<F>, cv: Cv<'a, Self::V>) -> ValXs<'a, Self::V> {
        use alloc::string::ToString;
        use core::iter::once;
        match &lut.terms[self.0] {
            Ast::Id => box_once(Ok(cv.1)),
            Ast::ToString => box_once(match cv.1.as_str() {
                Some(_) => Ok(cv.1),
                None => Ok(Self::V::from(cv.1.to_string())),
            }),
            Ast::Int(n) => box_once(Ok(Self::V::from(*n))),
            Ast::Num(x) => box_once(Self::V::from_num(x).map_err(Exn::from)),
            Ast::Str(s) => box_once(Ok(Self::V::from(s.clone()))),
            Ast::Arr(f) => box_once(f.run(lut, cv).collect()),
            Ast::ObjEmpty => box_once(Self::V::from_map([]).map_err(Exn::from)),
            Ast::ObjSingle(k, v) => Box::new(
                Self::cartesian(k, v, lut, cv).map(|(k, v)| Ok(Self::V::from_map([(k?, v?)])?)),
            ),
            // TODO: write test for `try (break $x)`
            Ast::TryCatch(f, c) => {
                Box::new(f.run(lut, (cv.0.clone(), cv.1)).flat_map(move |y| match y {
                    Err(Exn(exn::Inner::Err(e))) => c.run(lut, (cv.0.clone(), e.into_val())),
                    y => box_once(y),
                }))
            }
            Ast::Neg(f) => Box::new(f.run(lut, cv).map(|v| Ok((-v?)?))),

            // `l | r`
            Ast::Pipe(l, None, r) => {
                flat_map_then_with(l.run(lut, (cv.0.clone(), cv.1)), cv.0, move |y, ctx| {
                    r.run(lut, (ctx, y))
                })
            }
            // `l as $x | r`, `l as [...] | r`, or `l as {...} | r`
            Ast::Pipe(l, Some(pat), r) => l.pipe(lut, cv, move |(ctx, v), y| match pat {
                Pattern::Var => r.run(lut, (ctx.cons_var(y), v)),
                Pattern::Idx(pats) => {
                    let r = |ctx, v| r.run(lut, (ctx, v));
                    flat_map_then_with(bind_pats(pats, lut, ctx.clone(), (ctx, y)), v, r)
                }
            }),

            Ast::Comma(l, r) => Box::new(l.run(lut, cv.clone()).chain(lazy(|| r.run(lut, cv)))),
            Ast::Alt(l, r) => {
                let mut l = l
                    .run(lut, cv.clone())
                    .filter(|v| v.as_ref().map_or(true, ValT::as_bool));
                match l.next() {
                    Some(head) => Box::new(once(head).chain(l)),
                    None => r.run(lut, cv),
                }
            }
            Ast::Ite(if_, then_, else_) => if_.pipe(lut, cv, move |cv, v| {
                if v.as_bool() { then_ } else { else_ }.run(lut, cv)
            }),
            Ast::Path(f, path) => {
                let path = path.map_ref(|i| {
                    let cv = cv.clone();
                    crate::into_iter::collect_if_once(move || i.run(lut, cv))
                });
                flat_map_then_with(f.run(lut, cv), path, |y, path| {
                    flat_map_then_with(path.explode(), y, |path, y| {
                        Box::new(path.run(y).map(|r| r.map_err(Exn::from)))
                    })
                })
            }

            Ast::Update(path, f) => path.update(
                lut,
                (cv.0.clone(), cv.1),
                Box::new(move |v| f.run(lut, (cv.0.clone(), v))),
            ),
            Ast::UpdateMath(path, op, f) => f.pipe(lut, cv, move |cv, y| {
                path.update(
                    lut,
                    cv,
                    Box::new(move |x| box_once(op.run(x, y.clone()).map_err(Exn::from))),
                )
            }),
            Ast::UpdateAlt(path, f) => f.pipe(lut, cv, move |cv, y| {
                path.update(
                    lut,
                    cv,
                    Box::new(move |x| box_once(Ok(if x.as_bool() { x } else { y.clone() }))),
                )
            }),
            Ast::Assign(path, f) => f.pipe(lut, cv, move |cv, y| {
                path.update(lut, cv, Box::new(move |_| box_once(Ok(y.clone()))))
            }),

            Ast::Logic(l, stop, r) => l.pipe(lut, cv, move |cv, l| {
                if l.as_bool() == *stop {
                    box_once(Ok(Self::V::from(*stop)))
                } else {
                    Box::new(r.run(lut, cv).map(|r| Ok(Self::V::from(r?.as_bool()))))
                }
            }),
            Ast::Math(l, op, r) => {
                Box::new(Self::cartesian(l, r, lut, cv).map(|(x, y)| Ok(op.run(x?, y?)?)))
            }
            Ast::Cmp(l, op, r) => Box::new(
                Self::cartesian(l, r, lut, cv).map(|(x, y)| Ok(Self::V::from(op.run(&x?, &y?)))),
            ),

            Ast::Fold(xs, pat, init, update, fold_type) => {
                let xs = rc_lazy_list::List::from_iter(run_and_bind(xs, lut, cv.clone(), pat));
                let init = init.run(lut, cv.clone());
                let update = |ctx, v| update.run(lut, (ctx, v));
                let inner = |_, y: &Self::V| Some(y.clone());
                let inner_proj = |ctx, y: &Self::V| Some((ctx, y.clone()));
                flat_map_then_with(init, xs, move |i, xs| match fold_type {
                    Fold::Reduce => Box::new(fold(xs, i, update, |_| (), |_, _| None, Some)),
                    Fold::Foreach(None) => Box::new(fold(xs, i, update, |_| (), inner, |_| None)),
                    Fold::Foreach(Some(proj)) => flat_map_then(
                        fold(xs, i, update, |ctx| ctx.clone(), inner_proj, |_| None),
                        |(ctx, y)| proj.run(lut, (ctx, y)),
                    ),
                })
            }

            Ast::Var(v) => match cv.0.vars.get(*v).unwrap() {
                Bind2::Var(v) => box_once(Ok(v.clone())),
                Bind2::Fun((id, vars)) => id.run(lut, (cv.0.with_vars(vars.clone()), cv.1)),
                Bind2::Label(l) => box_once(Err(Exn(exn::Inner::Break(*l)))),
            },
            Ast::CallDef(id, args, skip, tailrec) => {
                use core::ops::ControlFlow;
                let with_vars = move |vars| Ctx {
                    vars,
                    labels: cv.0.labels,
                    inputs: cv.0.inputs,
                };
                let cvs = bind_vars(args, lut, cv.0.clone().skip_vars(*skip), cv);
                match tailrec {
                    None => flat_map_then(cvs, |cv| id.run(lut, cv)),
                    Some(Tailrec::Catch) => Box::new(crate::Stack::new(
                        [flat_map_then(cvs, |cv| id.run(lut, cv))].into(),
                        move |r| match r {
                            Err(Exn(exn::Inner::TailCall(id_, vars, v))) if id == id_ => {
                                ControlFlow::Continue(id.run(lut, (with_vars(vars), v)))
                            }
                            Ok(_) | Err(_) => ControlFlow::Break(r),
                        },
                    )),
                    Some(Tailrec::Throw) => Box::new(cvs.map(move |cv| {
                        cv.and_then(|cv| Err(Exn(exn::Inner::TailCall(id, cv.0.vars, cv.1))))
                    })),
                }
            }
            Ast::Native(id, args) => {
                let cvs = bind_vars(args, lut, Ctx::new([], cv.0.inputs), cv);
                flat_map_then(cvs, |cv| lut.funs[*id].run(lut, cv))
            }
            Ast::Label(id) => {
                let ctx = cv.0.cons_label();
                let labels = ctx.labels;
                Box::new(id.run(lut, (ctx, cv.1)).map_while(move |y| match y {
                    Err(Exn(exn::Inner::Break(b))) if b == labels => None,
                    y => Some(y),
                }))
            }
        }
    }

    fn update<'a>(
        &'a self,
        lut: &'a Lut<F>,
        cv: Cv<'a, Self::V>,
        f: BoxUpdate<'a, Self::V>,
    ) -> ValXs<'a, Self::V> {
        let err = box_once(Err(Exn::from(Error::path_expr())));
        match &lut.terms[self.0] {
            Ast::ToString => err,
            Ast::Int(_) | Ast::Num(_) | Ast::Str(_) => err,
            Ast::Arr(_) | Ast::ObjEmpty | Ast::ObjSingle(..) => err,
            Ast::Neg(_) | Ast::Logic(..) | Ast::Math(..) | Ast::Cmp(..) => err,
            Ast::Update(..) | Ast::UpdateMath(..) | Ast::UpdateAlt(..) | Ast::Assign(..) => err,
            // jq implements updates on `try ... catch` and `label`, but
            // I do not see how to implement this in jaq
            // folding, however, could be done, even if jq does not support it
            Ast::TryCatch(..) | Ast::Label(..) | Ast::Fold(..) => err,

            Ast::Id => f(cv.1),
            Ast::Path(l, path) => {
                let path = path.map_ref(|i| {
                    let cv = cv.clone();
                    crate::into_iter::collect_if_once(move || i.run(lut, cv))
                });
                let f = move |v| {
                    let mut paths = path.clone().explode();
                    box_once(paths.try_fold(v, |acc, path| path?.update(acc, &f)))
                };
                l.update(lut, cv, Box::new(f))
            }
            Ast::Pipe(l, None, r) => l.update(
                lut,
                (cv.0.clone(), cv.1),
                Box::new(move |v| r.update(lut, (cv.0.clone(), v), f.clone())),
            ),
            Ast::Pipe(l, Some(pat), r) => reduce(
                run_and_bind(l, lut, (cv.0, cv.1.clone()), pat),
                cv.1,
                move |ctx, v| r.update(lut, (ctx, v), f.clone()),
            ),
            Ast::Comma(l, r) => flat_map_then_with(
                l.update(lut, (cv.0.clone(), cv.1), f.clone()),
                (cv.0, f),
                move |v, (ctx, f)| r.update(lut, (ctx, v), f),
            ),
            Ast::Ite(if_, then_, else_) => reduce(if_.run(lut, cv.clone()), cv.1, move |x, v| {
                if x.as_bool() { then_ } else { else_ }.update(lut, (cv.0.clone(), v), f.clone())
            }),
            Ast::Alt(l, r) => {
                let some_true = l
                    .run(lut, cv.clone())
                    .any(|y| y.map_or(true, |y| y.as_bool()));
                if some_true { l } else { r }.update(lut, cv, f)
            }

            Ast::Var(v) => match cv.0.vars.get(*v).unwrap() {
                Bind2::Var(_) => err,
                Bind2::Fun(l) => l.0.update(lut, (cv.0.with_vars(l.1.clone()), cv.1), f),
                Bind2::Label(l) => box_once(Err(Exn(exn::Inner::Break(*l)))),
            },
            Ast::CallDef(id, args, skip, _tailrec) => {
                let init = cv.1.clone();
                let cvs = bind_vars(args, lut, cv.0.clone().skip_vars(*skip), cv);
                reduce(cvs, init, move |cv, v| id.update(lut, (cv.0, v), f.clone()))
            }
            Ast::Native(id, args) => {
                let init = cv.1.clone();
                let cvs = bind_vars(args, lut, Ctx::new([], cv.0.inputs), cv);
                reduce(cvs, init, move |cv, v| {
                    lut.funs[*id].update(lut, (cv.0, v), f.clone())
                })
            }
        }
    }
}

/// Function from a value to a stream of value results.
///
/// `F` is the type of (natively implemented) filter functions.
pub trait FilterT<F: FilterT<F, V = Self::V> = Self> {
    /// Type of values that the filter takes and yields.
    ///
    /// This is an associated type because it is strictly determined by `F`.
    type V: ValT;

    /// `f.run((c, v))` returns the output of `v | f` in the context `c`.
    fn run<'a>(&'a self, lut: &'a Lut<F>, cv: Cv<'a, Self::V>) -> ValXs<'a, Self::V>;

    /// `p.update((c, v), f)` returns the output of `v | p |= f` in the context `c`.
    fn update<'a>(
        &'a self,
        lut: &'a Lut<F>,
        cv: Cv<'a, Self::V>,
        f: BoxUpdate<'a, Self::V>,
    ) -> ValXs<'a, Self::V>;

    /// For every value `v` returned by `self.run(cv)`, call `f(cv, v)` and return all results.
    ///
    /// This has a special optimisation for the case where only a single `v` is returned.
    /// In that case, we can consume `cv` instead of cloning it.
    fn pipe<'a, T: 'a>(
        &'a self,
        lut: &'a Lut<F>,
        cv: Cv<'a, Self::V>,
        f: impl Fn(Cv<'a, Self::V>, Self::V) -> Results<'a, T, Self::V> + 'a,
    ) -> Results<'a, T, Self::V> {
        flat_map_then_with(self.run(lut, cv.clone()), cv, move |y, cv| f(cv, y))
    }

    /// Run `self` and `r` and return the cartesian product of their outputs.
    fn cartesian<'a>(
        &'a self,
        r: &'a Self,
        lut: &'a Lut<F>,
        cv: Cv<'a, Self::V>,
    ) -> box_iter::BoxIter<'a, Pair<ValX<'a, Self::V>>> {
        flat_map_with(self.run(lut, cv.clone()), cv, move |l, cv| {
            map_with(r.run(lut, cv), l, |r, l| (l, r))
        })
    }
}

type Pair<T> = (T, T);
