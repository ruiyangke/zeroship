import { table, decimal, int64 } from "zero-migrate";

// Operational seed 1: the workforce and its current position assignments.
// Foreign keys (department, grade, manager, position) are all satisfied by the
// reference data seeded earlier.
export const name = "seed_employees";

export default {
  up() {
    table("employees").insert({
      rows: [
        {
          id: "emp_01arz3ndektsv4rrffq69g6001",
          dept_id: "dept_01arz3ndektsv4rrffq69g5002",
          grade_id: int64("4"),
          email: "ada@example.com",
          first_name: "Ada",
          last_name: "Lovelace",
          hire_date: "2019-03-01",
          base_salary: decimal("185000.00"),
          employment_type: "full_time",
          manager_id: null,
        },
        {
          id: "emp_01arz3ndektsv4rrffq69g6002",
          dept_id: "dept_01arz3ndektsv4rrffq69g5002",
          grade_id: int64("3"),
          email: "alan@example.com",
          first_name: "Alan",
          last_name: "Turing",
          hire_date: "2020-06-15",
          base_salary: decimal("120000.00"),
          employment_type: "full_time",
          manager_id: "emp_01arz3ndektsv4rrffq69g6001",
        },
        {
          id: "emp_01arz3ndektsv4rrffq69g6003",
          dept_id: "dept_01arz3ndektsv4rrffq69g5003",
          grade_id: int64("3"),
          email: "grace@example.com",
          first_name: "Grace",
          last_name: "Hopper",
          hire_date: "2018-01-10",
          base_salary: decimal("115000.00"),
          employment_type: "full_time",
          manager_id: null,
        },
        {
          id: "emp_01arz3ndektsv4rrffq69g6004",
          dept_id: "dept_01arz3ndektsv4rrffq69g5004",
          grade_id: int64("2"),
          email: "edsger@example.com",
          first_name: "Edsger",
          last_name: "Dijkstra",
          hire_date: "2021-09-01",
          base_salary: decimal("72000.00"),
          employment_type: "part_time",
          manager_id: null,
        },
        {
          id: "emp_01arz3ndektsv4rrffq69g6005",
          dept_id: "dept_01arz3ndektsv4rrffq69g5002",
          grade_id: int64("4"),
          email: "barbara@example.com",
          first_name: "Barbara",
          last_name: "Liskov",
          hire_date: "2022-02-01",
          base_salary: decimal("175000.00"),
          employment_type: "contract",
          manager_id: "emp_01arz3ndektsv4rrffq69g6001",
        },
      ],
    });

    table("employee_position_history").insert({
      rows: [
        { employee_id: "emp_01arz3ndektsv4rrffq69g6001", position_id: "01ARZ3NDEKTSV4RRFFQ69G7001", effective_from: "2019-03-01", effective_to: null },
        { employee_id: "emp_01arz3ndektsv4rrffq69g6002", position_id: "01ARZ3NDEKTSV4RRFFQ69G7002", effective_from: "2020-06-15", effective_to: null },
        { employee_id: "emp_01arz3ndektsv4rrffq69g6003", position_id: "01ARZ3NDEKTSV4RRFFQ69G7003", effective_from: "2018-01-10", effective_to: null },
        { employee_id: "emp_01arz3ndektsv4rrffq69g6004", position_id: "01ARZ3NDEKTSV4RRFFQ69G7004", effective_from: "2021-09-01", effective_to: null },
        { employee_id: "emp_01arz3ndektsv4rrffq69g6005", position_id: "01ARZ3NDEKTSV4RRFFQ69G7002", effective_from: "2022-02-01", effective_to: null },
      ],
    });
  },
};
