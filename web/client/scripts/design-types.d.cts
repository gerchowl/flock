export {};

declare global {
  /** Data and reflection boundaries return unknown, never unchecked design props. */
  interface JSON {
    parse(text: string, reviver?: (key: string, value: unknown) => unknown): unknown;
  }
  interface Body {
    json(): Promise<unknown>;
  }
  interface ArrayConstructor {
    isArray(value: unknown): value is unknown[];
  }
  namespace Reflect {
    function get(target: object, propertyKey: PropertyKey, receiver?: unknown): unknown;
  }

  interface ObjectConstructor {
    create(prototype: object | null, properties?: PropertyDescriptorMap): unknown;
    assign(target: object, ...sources: unknown[]): unknown;
    values<T>(target: { [key: string]: T } | ArrayLike<T>): T[];
    values(target: {}): unknown[];
    entries<T>(target: { [key: string]: T } | ArrayLike<T>): [string, T][];
    entries(target: {}): [string, unknown][];
    getOwnPropertyDescriptor(target: object, key: PropertyKey): TypedPropertyDescriptor<unknown> | undefined;
    getOwnPropertyDescriptor<T extends object, K extends keyof T>(target: T, key: K): TypedPropertyDescriptor<T[K]> | undefined;
    getOwnPropertyDescriptors(target: object): Record<PropertyKey, TypedPropertyDescriptor<unknown>>;
  }
}
