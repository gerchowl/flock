/** Installed only by the browser conformance test, before application scripts. */
export function installCSSOMGuard() {
  const scope = { values: new WeakSet<object>(), depth: 0 };
  Object.assign(globalThis, { __designCSSOM: scope });
  const check = () => {
    if (scope.depth > 0) return;
    throw new Error("design CSSOM: mutation outside src/layout/primitives/");
  };
  for (const prototype of [CSSStyleDeclaration.prototype, HTMLElement.prototype, SVGElement.prototype]) {
    for (const key of Object.getOwnPropertyNames(prototype)) {
      const descriptor = Object.getOwnPropertyDescriptor(prototype, key)!;
      if (key === "style" && descriptor.get) {
        const getter = descriptor.get;
        const proxies = new WeakMap<object, CSSStyleDeclaration>();
        Object.defineProperty(prototype, key, {...descriptor, get() {
          const target = getter.call(this) as CSSStyleDeclaration;
          let proxy = proxies.get(target);
          if (!proxy) {
            proxy = new Proxy(target, {
              set(target, property, value) { check(); return Reflect.set(target, property, value, target); },
              get(target, property) { const value = Reflect.get(target, property, target); return typeof value === "function" ? value.bind(target) : value; },
            });
            proxies.set(target, proxy);
          }
          return proxy;
        }, ...(descriptor.set ? {set(value: unknown) {check(); descriptor.set!.call(this, value);}} : {})});
      } else if (descriptor.set && prototype === CSSStyleDeclaration.prototype) {
        const original = descriptor.set;
        Object.defineProperty(prototype, key, {...descriptor, set(value: unknown) { check(); original.call(this, value); }});
      }
    }
  }
  for (const key of ["setProperty", "removeProperty"] as const) {
    const original = CSSStyleDeclaration.prototype[key];
    Object.defineProperty(CSSStyleDeclaration.prototype, key, {configurable:true, value(this: CSSStyleDeclaration, ...args: unknown[]) {
      check(); return Reflect.apply(original, this, args);
    }});
  }
}
