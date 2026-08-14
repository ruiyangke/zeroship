import { table, decimal } from "zero-migrate";

// Operational seed 2: a payroll run with per-employee items (net_pay is the
// STORED generated column, never supplied), plus leave requests in every state.
export const name = "seed_operations";

export default {
  data() {
    table("payroll_runs").insert({
      rows: [
        { id: "01ARZ3NDEKTSV4RRFFQ69G8001", period_label: "2026-01", run_date: "2026-01-31", status: "approved" },
      ],
    });

    table("payroll_items").insert({
      rows: [
        { run_id: "01ARZ3NDEKTSV4RRFFQ69G8001", employee_id: "emp_01arz3ndektsv4rrffq69g6001", gross_pay: decimal("15416.67"), tax: decimal("5395.83") },
        { run_id: "01ARZ3NDEKTSV4RRFFQ69G8001", employee_id: "emp_01arz3ndektsv4rrffq69g6002", gross_pay: decimal("10000.00"), tax: decimal("2800.00") },
        { run_id: "01ARZ3NDEKTSV4RRFFQ69G8001", employee_id: "emp_01arz3ndektsv4rrffq69g6003", gross_pay: decimal("9583.33"), tax: decimal("2683.33") },
        { run_id: "01ARZ3NDEKTSV4RRFFQ69G8001", employee_id: "emp_01arz3ndektsv4rrffq69g6004", gross_pay: decimal("3000.00"), tax: decimal("600.00") },
        { run_id: "01ARZ3NDEKTSV4RRFFQ69G8001", employee_id: "emp_01arz3ndektsv4rrffq69g6005", gross_pay: decimal("14583.33"), tax: decimal("5104.17") },
      ],
    });

    table("leave_requests").insert({
      rows: [
        { employee_id: "emp_01arz3ndektsv4rrffq69g6002", leave_type: "vacation", start_date: "2026-02-10", end_date: "2026-02-14", days: decimal("5.0"), status: "approved" },
        { employee_id: "emp_01arz3ndektsv4rrffq69g6003", leave_type: "sick", start_date: "2026-01-20", end_date: "2026-01-20", days: decimal("1.0"), status: "approved" },
        { employee_id: "emp_01arz3ndektsv4rrffq69g6004", leave_type: "personal", start_date: "2026-03-01", end_date: "2026-03-01", days: decimal("0.5"), status: "pending" },
        { employee_id: "emp_01arz3ndektsv4rrffq69g6005", leave_type: "vacation", start_date: "2026-04-01", end_date: "2026-04-10", days: decimal("7.0"), status: "rejected" },
      ],
    });
  },
  irreversible:
    "inserts leave requests with database-generated UUIDs that are not recorded, so those rows cannot be identified for exact rollback",
};
