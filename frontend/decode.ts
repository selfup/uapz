/** Small JSON decoders for the desktop boundary, with bounded traversal. */
type Context = { remaining: number; depth: number };
export interface Schema<T> {
  read(value: unknown, path: string, context: Context): T;
}
type Value<S> = S extends Schema<infer T> ? T : never;
type Shape = Record<string, Schema<unknown>>;
type ObjectValue<S extends Shape> = {
  [K in keyof S as undefined extends Value<S[K]> ? never : K]: Value<S[K]>;
} & {
  [K in keyof S as undefined extends Value<S[K]> ? K : never]?: Value<S[K]>;
};

function fail(path: string, expected: string): never {
  // Report the location, never echo prompts, credentials or output values.
  throw new Error(`${path}: expected ${expected}`);
}
export function schema<T>(read: Schema<T>["read"]): Schema<T> {
  return {
    read(value, path, context) {
      if (--context.remaining < 0 || context.depth >= 64)
        throw new Error(`${path}: response exceeds the nesting or item limit`);
      context.depth++;
      try {
        return read(value, path, context);
      } finally {
        context.depth--;
      }
    },
  };
}
export const string = schema((value, path) =>
  typeof value === "string" ? value : fail(path, "text"),
);
export const number = schema((value, path) =>
  typeof value === "number" && Number.isFinite(value)
    ? value
    : fail(path, "a finite number"),
);
export const boolean = schema((value, path) =>
  typeof value === "boolean" ? value : fail(path, "a boolean"),
);
export const nil = schema((value, path) =>
  value === null ? null : fail(path, "null"),
);
export const integer = refine(
  number,
  (value) => Number.isSafeInteger(value) && value >= 0,
  "a nonnegative safe integer",
);
export function refine<T>(
  base: Schema<T>,
  valid: (value: T) => boolean,
  expected: string,
): Schema<T> {
  return schema((value, path, context) => {
    const parsed = base.read(value, path, context);
    return valid(parsed) ? parsed : fail(path, expected);
  });
}
export function enumeration<const T extends readonly string[]>(
  ...values: T
): Schema<T[number]> {
  return schema((value, path) => {
    const match = values.find((item) => item === value);
    return match === undefined
      ? fail(path, `one of ${values.join(", ")}`)
      : match;
  });
}
export function optional<T>(base: Schema<T>): Schema<T | undefined> {
  return schema((value, path, context) =>
    value === undefined ? undefined : base.read(value, path, context),
  );
}
export function nullable<T>(base: Schema<T>): Schema<T | null> {
  return schema((value, path, context) =>
    value === null ? null : base.read(value, path, context),
  );
}
export function array<T>(item: Schema<T>): Schema<T[]> {
  return schema((value, path, context) => {
    if (!Array.isArray(value)) return fail(path, "an array");
    if (value.length > context.remaining) return fail(path, "a bounded array");
    return Array.from(value, (child, index) =>
      item.read(child, `${path}[${index}]`, context),
    );
  });
}
function fields(value: unknown, path: string): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value))
    return fail(path, "an object");
  const prototype = Object.getPrototypeOf(value);
  if (prototype !== Object.prototype && prototype !== null)
    return fail(path, "a JSON object");
  return value as Record<string, unknown>;
}
export function object<S extends Shape>(shape: S): Schema<ObjectValue<S>> {
  return schema((value, path, context) => {
    const source = fields(value, path);
    const entries = Object.entries(shape).map(([key, child]) => [
      key,
      child.read(
        Object.hasOwn(source, key) ? source[key] : undefined,
        `${path}.${key}`,
        context,
      ),
    ]);
    // fromEntries creates own data properties, including __proto__.
    return Object.fromEntries(
      entries.filter(([, value]) => value !== undefined),
    ) as ObjectValue<S>;
  });
}
export function record<T>(item: Schema<T>): Schema<Record<string, T>> {
  return schema((value, path, context) => {
    const entries = Object.entries(fields(value, path));
    if (entries.length > context.remaining)
      return fail(path, "a bounded object");
    // Dictionary names are user data. A missing constructor/__proto__ must
    // behave like any other missing key, including after optional chaining.
    return Object.setPrototypeOf(
      Object.fromEntries(
        entries.map(([key, child], index) => [
          key,
          item.read(child, `${path}[entry ${index}]`, context),
        ]),
      ),
      null,
    );
  });
}
export function variant<S extends Shape>(
  key: string,
  variants: S,
): Schema<Value<S[keyof S]>> {
  return schema((value, path, context) => {
    const source = fields(value, path);
    const kind = Object.hasOwn(source, key) ? source[key] : undefined;
    const selected =
      typeof kind === "string" && Object.hasOwn(variants, kind)
        ? variants[kind]
        : undefined;
    if (!selected)
      return fail(
        `${path}.${key}`,
        `one of ${Object.keys(variants).join(", ")}`,
      );
    return selected.read(value, path, context) as Value<S[keyof S]>;
  });
}
export function lazy<T>(get: () => Schema<T>): Schema<T> {
  return schema((value, path, context) => get().read(value, path, context));
}
/** History records have independent native size limits. Don't charge an entire
 * history listing against the traversal budget of a single document. */
export function independent<T>(rule: Schema<T>): Schema<T> {
  return schema((value, path, context) =>
    rule.read(value, path, {
      remaining: 8_000_000,
      depth: context.depth,
    }),
  );
}
export function decode<T>(rule: Schema<T>, value: unknown): T {
  return rule.read(value, "response", { remaining: 8_000_000, depth: 0 });
}
