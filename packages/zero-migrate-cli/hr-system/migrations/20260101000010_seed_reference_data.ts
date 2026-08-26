import { table, decimal, int64 } from "zero-migrate";

// Reference data: departments (idempotent upsert by code), job grades, and
// positions. Public keys are supplied explicitly, as production seeds must be.
export const name = "seed_reference_data";

export default {
  data() {
    table("departments").insert({
      rows: [
        { id: "dept_01arz3ndektsv4rrffq69g5001", code: "HQ", name: "Headquarters" },
        { id: "dept_01arz3ndektsv4rrffq69g5002", code: "ENG", name: "Engineering" },
        { id: "dept_01arz3ndektsv4rrffq69g5003", code: "SALES", name: "Sales" },
        { id: "dept_01arz3ndektsv4rrffq69g5004", code: "HR", name: "People Ops" },
      ],
    });

    table("job_grades").insert({
      rows: [
        { id: int64("1"), grade_code: "G1", min_salary: decimal("40000.00"), max_salary: decimal("60000.00") },
        { id: int64("2"), grade_code: "G2", min_salary: decimal("60000.00"), max_salary: decimal("90000.00") },
        { id: int64("3"), grade_code: "G3", min_salary: decimal("90000.00"), max_salary: decimal("130000.00") },
        { id: int64("4"), grade_code: "G4", min_salary: decimal("130000.00"), max_salary: decimal("200000.00") },
      ],
    });

    table("positions").insert({
      rows: [
        { id: "01ARZ3NDEKTSV4RRFFQ69G7001", title: "Chief Executive", department_scope: "ALL", is_leadership: true },
        { id: "01ARZ3NDEKTSV4RRFFQ69G7002", title: "Software Engineer", department_scope: "ENG", is_leadership: false },
        { id: "01ARZ3NDEKTSV4RRFFQ69G7003", title: "Account Executive", department_scope: "SALES", is_leadership: false },
        { id: "01ARZ3NDEKTSV4RRFFQ69G7004", title: "People Manager", department_scope: "HR", is_leadership: false },
      ],
    });
  },
  inverse() {
    table("positions").delete({
      where: (col) =>
        col("id").in([
          "01ARZ3NDEKTSV4RRFFQ69G7001",
          "01ARZ3NDEKTSV4RRFFQ69G7002",
          "01ARZ3NDEKTSV4RRFFQ69G7003",
          "01ARZ3NDEKTSV4RRFFQ69G7004",
        ]),
    });
    table("job_grades").delete({
      where: (col) => col("id").in(["1", "2", "3", "4"]),
    });
    table("departments").delete({
      where: (col) =>
        col("id").in([
          "dept_01arz3ndektsv4rrffq69g5001",
          "dept_01arz3ndektsv4rrffq69g5002",
          "dept_01arz3ndektsv4rrffq69g5003",
          "dept_01arz3ndektsv4rrffq69g5004",
        ]),
    });
  },
};
