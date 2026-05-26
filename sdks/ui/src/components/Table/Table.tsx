import {
  type ReactNode,
  type TableHTMLAttributes,
} from "react";
import clsx from "clsx";

export interface TableColumn<T> {
  key: keyof T | string;
  header: ReactNode;
  cell?: (row: T) => ReactNode;
  align?: "start" | "end";
}

export interface TableProps<T extends Record<string, unknown>>
  extends Omit<TableHTMLAttributes<HTMLTableElement>, "children"> {
  columns: TableColumn<T>[];
  rows: T[];
  getRowKey?: (row: T, index: number) => string;
  empty?: ReactNode;
}

export function Table<T extends Record<string, unknown>>({
  columns,
  rows,
  getRowKey,
  empty = "No records yet.",
  className,
  ...props
}: TableProps<T>) {
  return (
    <div className="zs-table-wrap">
      <table className={clsx("zs-table", className)} {...props}>
        <thead>
          <tr>
            {columns.map((column) => (
              <th
                key={String(column.key)}
                className={clsx(column.align === "end" && "zs-table__cell--end")}
              >
                {column.header}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.length === 0 ? (
            <tr>
              <td colSpan={columns.length}>{empty}</td>
            </tr>
          ) : (
            rows.map((row, index) => (
              <tr key={getRowKey?.(row, index) ?? String(index)}>
                {columns.map((column) => (
                  <td
                    key={String(column.key)}
                    className={clsx(column.align === "end" && "zs-table__cell--end")}
                  >
                    {column.cell ? column.cell(row) : String(row[column.key] ?? "")}
                  </td>
                ))}
              </tr>
            ))
          )}
        </tbody>
      </table>
    </div>
  );
}
