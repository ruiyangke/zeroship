/**
 * HR System — full-featured example using @zeroship/db
 *
 * Models: employees, departments, positions, reviews, leave_requests, payroll
 *
 * Deploy:
 *   cd examples/hr-system && npm install
 *   zeroship deploy . --app=<uuid> --control=http://localhost:9090 --key=<key>
 */

import { model } from "@zeroship/db";

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

const departments = model("departments", {
  name:        { type: String, required: true },
  code:        { type: String, required: true },
  manager_id:  { type: Number },
  budget:      { type: Number, default: 0 },
  headcount:   { type: Number, default: 0 },
});

const positions = model("positions", {
  title:         { type: String, required: true },
  department_id: { type: Number, required: true },
  level:         { type: String, enum: ["junior", "mid", "senior", "lead", "director", "vp", "c-level"] },
  salary_min:    { type: Number },
  salary_max:    { type: Number },
  is_open:       { type: Boolean, default: true },
});

const employees = model("employees", {
  first_name:    { type: String, required: true },
  last_name:     { type: String, required: true },
  email:         { type: String, required: true },
  phone:         { type: String },
  department_id: { type: Number },
  position_id:   { type: Number },
  manager_id:    { type: Number },
  hire_date:     { type: Number },
  salary:        { type: Number },
  status:        { type: String, enum: ["active", "on_leave", "terminated"], default: "active" },
  skills:        { type: [String] },
});

const reviews = model("reviews", {
  employee_id:  { type: Number, required: true },
  reviewer_id:  { type: Number, required: true },
  period:       { type: String, required: true },
  rating:       { type: Number, required: true, min: 1, max: 5 },
  strengths:    { type: String },
  improvements: { type: String },
  goals:        { type: String },
  status:       { type: String, enum: ["draft", "submitted", "acknowledged"], default: "draft" },
});

const leave_requests = model("leave_requests", {
  employee_id: { type: Number, required: true },
  type:        { type: String, required: true, enum: ["vacation", "sick", "personal", "parental", "bereavement"] },
  start_date:  { type: Number, required: true },
  end_date:    { type: Number, required: true },
  days:        { type: Number, required: true },
  reason:      { type: String },
  status:      { type: String, enum: ["pending", "approved", "denied", "cancelled"], default: "pending" },
  approved_by: { type: Number },
});

const payroll = model("payroll", {
  employee_id:  { type: Number, required: true },
  period:       { type: String, required: true },
  base_salary:  { type: Number, required: true },
  bonus:        { type: Number, default: 0 },
  deductions:   { type: Number, default: 0 },
  net_pay:      { type: Number, required: true },
  status:       { type: String, enum: ["pending", "processed", "paid"], default: "pending" },
});

// ---------------------------------------------------------------------------
// Department API
// ---------------------------------------------------------------------------

export async function createDepartment(name: string, code: string, budget?: number) {
  return departments.create({ name, code, ...(budget && { budget }) });
}

export async function getDepartments() {
  return departments.find({}).sort({ name: 1 });
}

export async function getDepartment(id: number) {
  return departments.findOne({ id });
}

export async function updateDepartment(id: number, changes: Record<string, unknown>) {
  return departments.updateOne({ id }, changes);
}

// ---------------------------------------------------------------------------
// Position API
// ---------------------------------------------------------------------------

export async function createPosition(
  title: string, department_id: number, level: string,
  salary_min: number, salary_max: number
) {
  return positions.create({ title, department_id, level, salary_min, salary_max });
}

export async function getOpenPositions() {
  return positions.find({ is_open: true }).sort({ title: 1 });
}

export async function getPositionsByDepartment(department_id: number) {
  return positions.find({ department_id }).sort({ level: 1 });
}

export async function closePosition(id: number) {
  return positions.updateOne({ id }, { is_open: false });
}

// ---------------------------------------------------------------------------
// Employee API
// ---------------------------------------------------------------------------

export async function hireEmployee(
  first_name: string, last_name: string, email: string,
  department_id: number, position_id: number, salary: number
) {
  const { data, error } = await employees.create({
    first_name, last_name, email,
    department_id, position_id, salary,
    hire_date: Date.now(),
  });
  if (error) return { data: null, error };

  // Increment department headcount
  await departments.updateOne({ id: department_id }, { headcount: { $inc: 1 } });

  // Close the position
  await positions.updateOne({ id: position_id }, { is_open: false });

  return { data, error: null };
}

export async function getEmployees(filters?: Record<string, unknown>) {
  return employees.find(filters || {}).sort({ last_name: 1, first_name: 1 });
}

export async function getEmployee(id: number) {
  return employees.findOne({ id });
}

export async function getTeam(manager_id: number) {
  return employees.find({ manager_id, status: "active" }).sort({ last_name: 1 });
}

export async function updateEmployee(id: number, changes: Record<string, unknown>) {
  return employees.updateOne({ id }, changes);
}

export async function terminateEmployee(id: number) {
  const { data: emp } = await employees.findOne({ id });
  if (!emp) return { data: null, error: { message: "Employee not found" } };

  await employees.updateOne({ id }, { status: "terminated" });

  // Decrement department headcount
  if (emp.department_id) {
    await departments.updateOne(
      { id: emp.department_id as number },
      { headcount: { $inc: -1 } }
    );
  }

  return { data: { terminated: true }, error: null };
}

export async function addSkill(id: number, skill: string) {
  return employees.updateOne({ id }, { skills: { $addToSet: skill } });
}

export async function removeSkill(id: number, skill: string) {
  return employees.updateOne({ id }, { skills: { $pull: skill } });
}

export async function searchEmployees(query: string) {
  return employees.find({ first_name: { $ilike: `%${query}%` } }).limit(20);
}

export async function getEmployeesByDepartment(department_id: number) {
  return employees.find({ department_id, status: "active" }).sort({ last_name: 1 });
}

// ---------------------------------------------------------------------------
// Performance Review API
// ---------------------------------------------------------------------------

export async function createReview(
  employee_id: number, reviewer_id: number, period: string,
  rating: number, strengths: string, improvements: string, goals: string
) {
  return reviews.create({
    employee_id, reviewer_id, period, rating,
    strengths, improvements, goals,
  });
}

export async function getReviewsForEmployee(employee_id: number) {
  return reviews.find({ employee_id }).sort({ createdAt: -1 });
}

export async function submitReview(id: number) {
  return reviews.updateOne({ id }, { status: "submitted" });
}

export async function acknowledgeReview(id: number) {
  return reviews.updateOne({ id }, { status: "acknowledged" });
}

export async function getReviewStats() {
  return reviews.aggregate([
    { $group: { _id: "$period", count: { $sum: 1 }, avg_rating: { $avg: "$rating" } } },
    { $sort: { _id: -1 } },
  ]);
}

// ---------------------------------------------------------------------------
// Leave Request API
// ---------------------------------------------------------------------------

export async function requestLeave(
  employee_id: number, type: string,
  start_date: number, end_date: number, days: number, reason?: string
) {
  return leave_requests.create({
    employee_id, type, start_date, end_date, days,
    ...(reason && { reason }),
  });
}

export async function getPendingLeaves() {
  return leave_requests.find({ status: "pending" }).sort({ createdAt: 1 });
}

export async function getLeavesByEmployee(employee_id: number) {
  return leave_requests.find({ employee_id }).sort({ start_date: -1 });
}

export async function approveLeave(id: number, approved_by: number) {
  const { data } = await leave_requests.updateOne(
    { id, status: "pending" },
    { status: "approved", approved_by }
  );

  // Set employee status to on_leave
  if (data && data.modifiedCount > 0) {
    const { data: leave } = await leave_requests.findOne({ id });
    if (leave) {
      await employees.updateOne(
        { id: leave.employee_id as number },
        { status: "on_leave" }
      );
    }
  }

  return { data, error: null };
}

export async function denyLeave(id: number, approved_by: number) {
  return leave_requests.updateOne(
    { id, status: "pending" },
    { status: "denied", approved_by }
  );
}

export async function getLeaveBalance(employee_id: number) {
  const { data: approved } = await leave_requests.find({
    employee_id,
    status: "approved",
  });
  const used = (approved || []).reduce(
    (acc: Record<string, number>, l: Record<string, unknown>) => {
      const t = l.type as string;
      acc[t] = (acc[t] || 0) + (l.days as number);
      return acc;
    },
    {} as Record<string, number>
  );
  return {
    data: {
      vacation: { used: used.vacation || 0, total: 20 },
      sick: { used: used.sick || 0, total: 10 },
      personal: { used: used.personal || 0, total: 5 },
    },
    error: null,
  };
}

// ---------------------------------------------------------------------------
// Payroll API
// ---------------------------------------------------------------------------

export async function runPayroll(period: string) {
  const { data: activeEmployees } = await employees.find({ status: "active" });
  if (!activeEmployees || activeEmployees.length === 0) {
    return { data: { processed: 0 }, error: null };
  }

  let processed = 0;
  for (const emp of activeEmployees) {
    const base = (emp.salary as number) || 0;
    const deductions = Math.round(base * 0.3); // 30% tax + benefits
    const net = base - deductions;

    await payroll.create({
      employee_id: emp._id as number,
      period,
      base_salary: base,
      deductions,
      net_pay: net,
    });
    processed++;
  }

  return { data: { processed, period }, error: null };
}

export async function getPayrollByPeriod(period: string) {
  return payroll.find({ period }).sort({ employee_id: 1 });
}

export async function getPayrollByEmployee(employee_id: number) {
  return payroll.find({ employee_id }).sort({ createdAt: -1 });
}

export async function processPayroll(period: string) {
  return payroll.updateMany(
    { period, status: "pending" },
    { status: "processed" }
  );
}

export async function markPaid(period: string) {
  return payroll.updateMany(
    { period, status: "processed" },
    { status: "paid" }
  );
}

// ---------------------------------------------------------------------------
// Analytics / Dashboard
// ---------------------------------------------------------------------------

export async function getDashboard() {
  const { data: totalEmployees } = await employees.countDocuments({ status: "active" });
  const { data: totalDepartments } = await departments.countDocuments({});
  const { data: openPositions } = await positions.countDocuments({ is_open: true });
  const { data: pendingLeaves } = await leave_requests.countDocuments({ status: "pending" });

  return {
    data: {
      totalEmployees,
      totalDepartments,
      openPositions,
      pendingLeaves,
    },
    error: null,
  };
}

export async function getHeadcountByDepartment() {
  return employees.aggregate([
    { $match: { status: "active" } },
    { $group: { _id: "$department_id", headcount: { $sum: 1 }, avg_salary: { $avg: "$salary" } } },
    { $sort: { headcount: -1 } },
  ]);
}

export async function getSalaryDistribution() {
  return employees.aggregate([
    { $match: { status: "active" } },
    { $group: {
      _id: "$department_id",
      min_salary: { $min: "$salary" },
      max_salary: { $max: "$salary" },
      avg_salary: { $avg: "$salary" },
      total_payroll: { $sum: "$salary" },
      headcount: { $sum: 1 },
    }},
    { $sort: { total_payroll: -1 } },
  ]);
}

export async function getAttritionReport() {
  const { data: terminated } = await employees.countDocuments({ status: "terminated" });
  const { data: total } = await employees.countDocuments({});
  const rate = total ? ((terminated || 0) / total * 100).toFixed(1) : "0.0";
  return { data: { terminated, total, attrition_rate: `${rate}%` }, error: null };
}

export async function getSkillsInventory() {
  return employees.distinct("skills", { status: "active" });
}
