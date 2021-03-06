use std::pin::Pin;
use std::sync::Arc;
use dbus::channel::Sender;
use dbus::arg;
use std::future::Future;
use std::marker::PhantomData;
use crate::{Context, MethodErr, IfaceBuilder,stdimpl};
use crate::ifacedesc::Registry;
use std::collections::{BTreeMap, HashSet};
use std::any::Any;
use std::fmt;

const INTROSPECTABLE: usize = 0;
const PROPERTIES: usize = 1;

#[derive(Debug, Copy, Clone, Eq, Ord, Hash, PartialEq, PartialOrd)]
pub struct IfaceToken<T: Send + 'static>(usize, PhantomData<&'static T>);


#[derive(Debug)]
struct Object {
    ifaces: HashSet<usize>,
    data: Box<dyn Any + Send + 'static>
}

pub type BoxedSpawn = Box<dyn Fn(Pin<Box<dyn Future<Output = ()> + Send + 'static>>) + Send + 'static>;

struct AsyncSupport {
    sender: Arc<dyn Sender + Send + Sync + 'static>,
    spawner: BoxedSpawn,
}

impl fmt::Debug for AsyncSupport {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { write!(f, "AsyncSupport") }
}

#[derive(Debug)]
pub struct Crossroads {
    map: BTreeMap<dbus::Path<'static>, Object>,
    registry: Registry,
    add_standard_ifaces: bool,
    async_support: Option<AsyncSupport>,
}

impl Crossroads {
    pub fn new() -> Crossroads {
        let mut cr = Crossroads {
            map: Default::default(),
            registry: Default::default(),
            add_standard_ifaces: true,
            async_support: None,
        };
        let t0 = stdimpl::introspectable(&mut cr);
        let t1 = stdimpl::properties(&mut cr);
        debug_assert_eq!(t0.0, INTROSPECTABLE);
        debug_assert_eq!(t1.0, PROPERTIES);
        cr
    }

    pub fn set_add_standard_ifaces(&mut self, enable: bool) {
        self.add_standard_ifaces = enable;
    }

    pub fn register<T, N, F>(&mut self, name: N, f: F) -> IfaceToken<T>
    where T: Send + 'static, N: Into<dbus::strings::Interface<'static>>,
    F: FnOnce(&mut IfaceBuilder<T>)
    {
        let iface = IfaceBuilder::build(Some(name.into()), f);
        let x = self.registry.push(iface);
        IfaceToken(x, PhantomData)
    }

    pub fn data_mut<D: Any + Send + 'static>(&mut self, name: &dbus::Path<'static>) -> Option<&mut D> {
        let obj = self.map.get_mut(name)?;
        obj.data.downcast_mut()
    }

    pub fn insert<'z, D, I, N>(&mut self, name: N, ifaces: I, data: D)
    where D: Any + Send + 'static, N: Into<dbus::Path<'static>>, I: IntoIterator<Item = &'z IfaceToken<D>>
    {
        let ifaces = ifaces.into_iter().map(|x| x.0);
        let mut ifaces: HashSet<usize> = std::iter::FromIterator::from_iter(ifaces);
        if self.add_standard_ifaces {
            ifaces.insert(INTROSPECTABLE);
            if ifaces.iter().any(|u| self.registry().has_props(*u)) {
                ifaces.insert(PROPERTIES);
            }
        }
        self.map.insert(name.into(), Object { ifaces, data: Box::new(data)});
    }

    pub (crate) fn find_iface_token(&self,
        path: &dbus::Path<'static>,
        interface: Option<&dbus::strings::Interface<'static>>)
    -> Result<usize, MethodErr> {
        let obj = self.map.get(path).ok_or_else(|| MethodErr::no_path(path))?;
        self.registry.find_token(interface, &obj.ifaces)
    }

    pub (crate) fn registry(&mut self) -> &mut Registry { &mut self.registry }

    pub (crate) fn registry_and_ifaces(&self, path: &dbus::Path<'static>)
    -> (&Registry, &HashSet<usize>) {
        let obj = self.map.get(path).unwrap();
        (&self.registry, &obj.ifaces)
    }

    pub (crate) fn get_children(&self, path: &dbus::Path<'static>) -> Vec<&str> {
        use std::ops::Bound;
        let mut range = self.map.range((Bound::Excluded(path), Bound::Unbounded));
        let p2 = path.as_bytes();
        let mut r = vec!();
        while let Some((c, _)) = range.next() {
            if !c.as_bytes().starts_with(p2) { break; }
            let csub: &str = &c[p2.len()..];
            if csub.len() == 0 || csub.as_bytes()[0] != b'/' { continue; }
            r.push(&csub[1..]);
        };
        r
    }

    pub (crate) fn run_async_method<F, R>(&mut self, mut ctx: Context, f: F)
    where F: FnOnce(Context, &mut Crossroads) -> R,
    R: Future<Output=()> + Send + 'static
    {
        let sender = self.async_support.as_ref().expect("Async support not set").sender.clone();
        ctx.set_on_drop(sender);
        let future = f(ctx, self);
        let spawner = &self.async_support.as_ref().expect("Async support not set").spawner;
        let boxed = Box::pin(async move { future.await });
        (spawner)(boxed)
    }

    pub fn handle_message<S: dbus::channel::Sender>(&mut self, message: dbus::Message, conn: &S) -> Result<(), ()> {
        let mut ctx = Context::new(message).ok_or(())?;
        let (itoken, mut cb) = ctx.check(|ctx| {
            let itoken = self.find_iface_token(ctx.path(), ctx.interface())?;
            let cb = self.registry.take_method(itoken, ctx.method())?;
            Ok((itoken, cb))
        })?;
        // No failure paths before method is given back!
        let methodname = ctx.method().clone();
        let ctx = cb(ctx, self);
        self.registry.give_method(itoken, &methodname, cb);
        if let Some(mut ctx) = ctx { ctx.flush_messages(conn) } else { Ok(()) }
    }

    pub fn introspectable<T: Send + 'static>(&self) -> IfaceToken<T> { IfaceToken(INTROSPECTABLE, PhantomData) }
    pub fn properties<T: Send + 'static>(&self) -> IfaceToken<T> { IfaceToken(PROPERTIES, PhantomData) }

    pub fn spawn_method<OA: arg::AppendAll, F>(&self, mut ctx: Context, f: F) -> Result<PhantomData<OA>, Context>
    where F: Future<Output=Result<OA, MethodErr>> + Send + 'static {
        let support = match self.async_support.as_ref() {
            Some(x) => x,
            None => {
                let _ = ctx.check::<(),_>(|_| Err(MethodErr::failed(&"Async support not set")));
                return Err(ctx);
            }
        };
        let sender = support.sender.clone();
        let boxed = Box::pin(async move {
            let r = f.await;
            if let Ok(oa) = ctx.check(|_| {Ok(r?) }) {
                ctx.do_reply(|mut msg| oa.append(&mut arg::IterAppend::new(&mut msg)));
            }
            let _ = ctx.flush_messages(&*sender);
            ()
        });
        (support.spawner)(boxed);
        Ok(PhantomData)
    }

    pub fn set_async_support(&mut self, x: Option<(Arc<dyn Sender + Send + Sync + 'static>, BoxedSpawn)>) -> Option<(Arc<dyn Sender + Send + Sync + 'static>, BoxedSpawn)> {
        let a = self.async_support.take();
        self.async_support = x.map(|x| AsyncSupport {
            sender: x.0,
            spawner: x.1
        });
        a.map(|x| (x.sender, x.spawner))
    }
}
