import { ValidationError } from "../errors.js";
import { trackCollectionAccess } from "../live.js";
import { mapFilterOutbound, mapResultDoc } from "../utils.js";
import type { Filter, PlainObject, Result, Row, VectorMetric } from "../types.js";

export interface VectorGeoCollectionInternals<S> {
  _name: string;
  _softDelete: boolean;
  _run<T>(fn: () => Promise<T>): Promise<Result<T>>;
  _nativeCollection(): ZeroshipCollection;
  _toColumn(field: string): string;
  _toField(column: string): string;
}

/**
 * Validates `k` / `limit` arguments to `.search()` are positive integers
 * in `1..=1000`. Throws ValidationError with `code: "INVALID_K"` on
 * violation. The 1000-row ceiling matches the engine-side practical
 * limit for kNN flat scan + GIN/ivfflat result sets.
 */
export function _validateK(value: number, paramName: string): void {
  if (
    typeof value !== "number" ||
    !Number.isInteger(value) ||
    value < 1 ||
    value > 1000
  ) {
    throw new ValidationError({
      args: {
        path: paramName,
        message: `search: \`${paramName}\` must be an integer in 1..=1000 (got ${value})`,
      },
    });
  }
}

function _mergeFilter(
  self: Pick<VectorGeoCollectionInternals<unknown>, "_softDelete" | "_toColumn">,
  filter: ZeroshipDbFilter,
): ZeroshipDbFilter {
  if (!self._softDelete) return filter;
  const softFilter: ZeroshipDbFilter = { [self._toColumn("deleted_at")]: null };
  const hasKeys = Object.keys(filter).length > 0;
  return hasKeys ? ({ $and: [filter, softFilter] } as ZeroshipDbFilter) : softFilter;
}

/**
 * **P4** — vector-nearest-neighbour OR full-text search,
 * discriminated by the presence of `vector` vs. `text` in `args`.
 */
export function searchCollection<S>(
  self: VectorGeoCollectionInternals<S>,
  args:
    | {
        vector: number[];
        k?: number;
        metric?: VectorMetric;
        column?: string;
        filter?: Filter<S>;
      }
    | { text: string; limit?: number; k?: number; filter?: Filter<S> },
): Promise<Result<(Row<S> & { _distance?: number; _rank?: number })[]>> {
  trackCollectionAccess(self._name);
  return self._run(async () => {
    const nativeArgs: {
      vector?: number[];
      text?: string;
      k?: number;
      limit?: number;
      metric?: VectorMetric;
      column?: string;
      filter?: ZeroshipDbFilter;
    } = {};
    if ("vector" in args && args.vector !== undefined) {
      if (!Array.isArray(args.vector)) {
        throw new ValidationError({
          vector: {
            path: "vector",
            message: "search: `vector` must be a number[]",
          },
        });
      }
      nativeArgs.vector = args.vector;
      if (args.metric !== undefined) nativeArgs.metric = args.metric;
      if (args.column !== undefined) {
        nativeArgs.column = self._toColumn(args.column);
      }
    } else if ("text" in args && args.text !== undefined) {
      if (typeof args.text !== "string") {
        throw new ValidationError({
          text: { path: "text", message: "search: `text` must be a string" },
        });
      }
      nativeArgs.text = args.text;
      if ((args as { limit?: number }).limit !== undefined) {
        const lim = (args as { limit?: number }).limit as number;
        _validateK(lim, "limit");
        nativeArgs.limit = lim;
      }
    } else {
      throw new ValidationError({
        args: {
          path: "args",
          message: "search: args must include `vector` or `text`",
        },
      });
    }
    if (args.k !== undefined) {
      _validateK(args.k, "k");
      nativeArgs.k = args.k;
    }
    if (args.filter !== undefined) {
      nativeArgs.filter = _mergeFilter(
        self,
        mapFilterOutbound(args.filter as ZeroshipDbFilter, self._toColumn),
      );
    } else if (self._softDelete) {
      nativeArgs.filter = _mergeFilter(self, {});
    }
    const results = await self._nativeCollection().search(nativeArgs);
    return (results ?? []).map(
      (d) =>
        mapResultDoc(d as PlainObject, self._toField) as Row<S> & {
          _distance?: number;
          _rank?: number;
        },
    );
  });
}

/**
 * **P4 PR 3** — spatial within-radius search.
 */
export function nearCollection<S>(
  self: VectorGeoCollectionInternals<S>,
  args: {
    field: keyof S & string;
    point: { lat: number; lng: number };
    radius: number;
    filter?: Filter<S>;
    limit?: number;
  },
): Promise<Result<(Row<S> & { _distance_m: number })[]>> {
  trackCollectionAccess(self._name);
  return self._run(async () => {
    if (typeof args.field !== "string" || args.field.length === 0) {
      throw new ValidationError({
        field: {
          path: "field",
          message: "near: `field` must be a non-empty string",
        },
      });
    }
    if (
      args.point === null ||
      typeof args.point !== "object" ||
      typeof args.point.lat !== "number" ||
      typeof args.point.lng !== "number"
    ) {
      throw new ValidationError({
        point: {
          path: "point",
          message: "near: `point` must be `{ lat: number, lng: number }`",
        },
      });
    }
    if (args.point.lat < -90 || args.point.lat > 90) {
      throw new ValidationError({
        "point.lat": {
          path: "point.lat",
          message: "near: `point.lat` must be in [-90, 90]",
        },
      });
    }
    if (args.point.lng < -180 || args.point.lng > 180) {
      throw new ValidationError({
        "point.lng": {
          path: "point.lng",
          message: "near: `point.lng` must be in [-180, 180]",
        },
      });
    }
    if (
      typeof args.radius !== "number" ||
      !Number.isFinite(args.radius) ||
      args.radius <= 0
    ) {
      throw new ValidationError({
        radius: {
          path: "radius",
          message:
            "near: `radius` must be a positive finite number (metres)",
        },
      });
    }

    const nativeArgs: {
      field: string;
      point: { lat: number; lng: number };
      radius: number;
      filter?: ZeroshipDbFilter;
      limit?: number;
    } = {
      field: self._toColumn(args.field as string),
      point: { lat: args.point.lat, lng: args.point.lng },
      radius: args.radius,
    };
    if (args.limit !== undefined) nativeArgs.limit = args.limit;
    if (args.filter !== undefined) {
      nativeArgs.filter = _mergeFilter(
        self,
        mapFilterOutbound(args.filter as ZeroshipDbFilter, self._toColumn),
      );
    } else if (self._softDelete) {
      nativeArgs.filter = _mergeFilter(self, {});
    }

    const colAny = self._nativeCollection() as unknown as {
      near: (a: typeof nativeArgs) => Promise<PlainObject[]>;
    };
    const results = await colAny.near(nativeArgs);
    return (results ?? []).map(
      (d) =>
        mapResultDoc(d as PlainObject, self._toField) as Row<S> & {
          _distance_m: number;
        },
    );
  });
}
