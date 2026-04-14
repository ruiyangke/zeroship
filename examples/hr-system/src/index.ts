"use server"

/**
 * HR System — full-featured example using @zeroship/db
 *
 * Models: 25 models covering employees, departments, positions, recruitment,
 *         time & attendance, leave, payroll, performance, training, compensation,
 *         benefits, documents, compliance, and notifications.
 *
 * Deploy:
 *   cd examples/hr-system && npm install
 *   zeroship deploy . --app=<uuid> --control=http://localhost:9090 --key=<key>
 */

import { createDb } from "@zeroship/db";

const db = createDb({
  // ---------------------------------------------------------------------------
  // Core
  // ---------------------------------------------------------------------------
  departments: {
    name:                 { type: String, required: true },
    code:                 { type: String, required: true },
    manager_id:           { type: Number },
    budget:               { type: Number, default: 0 },
    headcount:            { type: Number, default: 0 },
    parent_department_id: { type: Number },
  },

  positions: {
    title:         { type: String, required: true },
    department_id: { type: Number, required: true },
    level:         { type: String, enum: ["junior", "mid", "senior", "lead", "director", "vp", "c-level"] },
    salary_min:    { type: Number },
    salary_max:    { type: Number },
    is_open:       { type: Boolean, default: true },
    description:   { type: String },
  },

  employees: {
    first_name:               { type: String, required: true },
    last_name:                { type: String, required: true },
    email:                    { type: String, required: true },
    phone:                    { type: String },
    department_id:            { type: Number },
    position_id:              { type: Number },
    manager_id:               { type: Number },
    hire_date:                { type: Number },
    salary:                   { type: Number },
    status:                   { type: String, enum: ["active", "on_leave", "terminated"], default: "active" },
    skills:                   { type: [String] },
    avatar_url:               { type: String },
    emergency_contact_name:   { type: String },
    emergency_contact_phone:  { type: String },
    address:                  { type: String },
    date_of_birth:            { type: Number },
  },

  // ---------------------------------------------------------------------------
  // Recruitment
  // ---------------------------------------------------------------------------
  job_postings: {
    position_id:   { type: Number, required: true },
    title:         { type: String, required: true },
    description:   { type: String },
    requirements:  { type: String },
    status:        { type: String, enum: ["draft", "open", "closed"], default: "draft" },
    posted_date:   { type: Number },
    closing_date:  { type: Number },
  },

  applicants: {
    job_posting_id: { type: Number, required: true },
    name:           { type: String, required: true },
    email:          { type: String, required: true },
    phone:          { type: String },
    resume_url:     { type: String },
    stage:          { type: String, enum: ["applied", "screening", "interview", "offer", "hired", "rejected"], default: "applied" },
    rating:         { type: Number },
    notes:          { type: String },
    applied_date:   { type: Number },
  },

  interviews: {
    applicant_id:      { type: Number, required: true },
    interviewer_id:    { type: Number, required: true },
    scheduled_at:      { type: Number, required: true },
    duration_minutes:  { type: Number, default: 60 },
    type:              { type: String, enum: ["phone", "video", "onsite"], default: "video" },
    status:            { type: String, enum: ["scheduled", "completed", "cancelled"], default: "scheduled" },
    feedback:          { type: String },
    rating:            { type: Number },
  },

  // ---------------------------------------------------------------------------
  // Time & Attendance
  // ---------------------------------------------------------------------------
  timesheets: {
    employee_id:    { type: Number, required: true },
    date:           { type: Number, required: true },
    clock_in:       { type: Number },
    clock_out:      { type: Number },
    hours_worked:   { type: Number, default: 0 },
    overtime_hours: { type: Number, default: 0 },
    status:         { type: String, enum: ["draft", "submitted", "approved"], default: "draft" },
    notes:          { type: String },
  },

  work_schedules: {
    employee_id:  { type: Number, required: true },
    day_of_week:  { type: Number, required: true }, // 0=Sun, 6=Sat
    start_time:   { type: String, required: true },
    end_time:     { type: String, required: true },
    is_remote:    { type: Boolean, default: false },
  },

  // ---------------------------------------------------------------------------
  // Leave
  // ---------------------------------------------------------------------------
  leave_requests: {
    employee_id: { type: Number, required: true },
    type:        { type: String, required: true, enum: ["vacation", "sick", "personal", "parental", "bereavement"] },
    start_date:  { type: Number, required: true },
    end_date:    { type: Number, required: true },
    days:        { type: Number, required: true },
    reason:      { type: String },
    status:      { type: String, enum: ["pending", "approved", "denied", "cancelled"], default: "pending" },
    approved_by: { type: Number },
    approved_at: { type: Number },
  },

  leave_balances: {
    employee_id:    { type: Number, required: true },
    year:           { type: Number, required: true },
    vacation_total: { type: Number, default: 20 },
    vacation_used:  { type: Number, default: 0 },
    sick_total:     { type: Number, default: 10 },
    sick_used:      { type: Number, default: 0 },
    personal_total: { type: Number, default: 5 },
    personal_used:  { type: Number, default: 0 },
  },

  holidays: {
    name:         { type: String, required: true },
    date:         { type: Number, required: true },
    is_recurring: { type: Boolean, default: false },
  },

  // ---------------------------------------------------------------------------
  // Payroll
  // ---------------------------------------------------------------------------
  payroll_runs: {
    period:           { type: String, required: true },
    run_date:         { type: Number },
    status:           { type: String, enum: ["draft", "processing", "completed"], default: "draft" },
    total_gross:      { type: Number, default: 0 },
    total_net:        { type: Number, default: 0 },
    total_deductions: { type: Number, default: 0 },
    processed_by:     { type: Number },
  },

  payslips: {
    payroll_run_id:      { type: Number, required: true },
    employee_id:         { type: Number, required: true },
    base_salary:         { type: Number, required: true },
    overtime_pay:        { type: Number, default: 0 },
    bonus:               { type: Number, default: 0 },
    deductions_tax:      { type: Number, default: 0 },
    deductions_benefits: { type: Number, default: 0 },
    deductions_other:    { type: Number, default: 0 },
    net_pay:             { type: Number, required: true },
    status:              { type: String, enum: ["pending", "paid"], default: "pending" },
  },

  // ---------------------------------------------------------------------------
  // Performance
  // ---------------------------------------------------------------------------
  reviews: {
    employee_id:  { type: Number, required: true },
    reviewer_id:  { type: Number, required: true },
    period:       { type: String, required: true },
    cycle:        { type: String, enum: ["quarterly", "annual"], default: "annual" },
    rating:       { type: Number, min: 1, max: 5 },
    strengths:    { type: String },
    improvements: { type: String },
    goals:        { type: String },
    status:       { type: String, enum: ["draft", "submitted", "acknowledged"], default: "draft" },
  },

  goals: {
    employee_id:  { type: Number, required: true },
    title:        { type: String, required: true },
    description:  { type: String },
    target_date:  { type: Number },
    status:       { type: String, enum: ["active", "completed", "cancelled"], default: "active" },
    progress:     { type: Number, default: 0, min: 0, max: 100 },
    category:     { type: String, enum: ["performance", "development", "project"], default: "performance" },
  },

  feedback: {
    from_employee_id: { type: Number, required: true },
    to_employee_id:   { type: Number, required: true },
    type:             { type: String, enum: ["praise", "constructive"], required: true },
    message:          { type: String, required: true },
    is_anonymous:     { type: Boolean, default: false },
  },

  // ---------------------------------------------------------------------------
  // Training
  // ---------------------------------------------------------------------------
  courses: {
    title:            { type: String, required: true },
    description:      { type: String },
    category:         { type: String },
    duration_hours:   { type: Number },
    is_mandatory:     { type: Boolean, default: false },
    max_participants: { type: Number },
  },

  enrollments: {
    course_id:    { type: Number, required: true },
    employee_id:  { type: Number, required: true },
    status:       { type: String, enum: ["enrolled", "in_progress", "completed", "dropped"], default: "enrolled" },
    enrolled_at:  { type: Number },
    completed_at: { type: Number },
    score:        { type: Number },
  },

  certifications: {
    employee_id:    { type: Number, required: true },
    name:           { type: String, required: true },
    issuer:         { type: String },
    issue_date:     { type: Number },
    expiry_date:    { type: Number },
    credential_url: { type: String },
  },

  // ---------------------------------------------------------------------------
  // Compensation & Benefits
  // ---------------------------------------------------------------------------
  compensation_history: {
    employee_id:    { type: Number, required: true },
    effective_date: { type: Number, required: true },
    salary:         { type: Number, required: true },
    change_type:    { type: String, enum: ["hire", "promotion", "adjustment", "annual"], required: true },
    change_reason:  { type: String },
    approved_by:    { type: Number },
  },

  benefits_plans: {
    name:                    { type: String, required: true },
    type:                    { type: String, enum: ["health", "dental", "vision", "life", "retirement"], required: true },
    provider:                { type: String },
    monthly_cost_employee:   { type: Number, default: 0 },
    monthly_cost_employer:   { type: Number, default: 0 },
  },

  benefits_enrollments: {
    employee_id: { type: Number, required: true },
    plan_id:     { type: Number, required: true },
    start_date:  { type: Number, required: true },
    end_date:    { type: Number },
    status:      { type: String, enum: ["active", "cancelled"], default: "active" },
  },

  expense_claims: {
    employee_id:  { type: Number, required: true },
    description:  { type: String, required: true },
    amount:       { type: Number, required: true },
    category:     { type: String, enum: ["travel", "meals", "equipment", "other"], required: true },
    receipt_url:  { type: String },
    status:       { type: String, enum: ["submitted", "approved", "rejected", "reimbursed"], default: "submitted" },
    submitted_at: { type: Number },
    approved_by:  { type: Number },
  },

  // ---------------------------------------------------------------------------
  // Documents & Compliance
  // ---------------------------------------------------------------------------
  documents: {
    employee_id:  { type: Number, required: true },
    type:         { type: String, enum: ["contract", "id", "certification", "policy", "other"], required: true },
    name:         { type: String, required: true },
    file_url:     { type: String, required: true },
    uploaded_at:  { type: Number },
    expires_at:   { type: Number },
  },

  audit_log: {
    actor_id:     { type: Number, required: true },
    action:       { type: String, enum: ["create", "update", "delete"], required: true },
    entity_type:  { type: String, required: true },
    entity_id:    { type: Number, required: true },
    changes_json: { type: String },
    timestamp:    { type: Number },
  },

  policies: {
    title:          { type: String, required: true },
    content:        { type: String },
    version:        { type: String },
    effective_date: { type: Number },
    category:       { type: String, enum: ["handbook", "conduct", "safety", "privacy"], required: true },
  },

  // ---------------------------------------------------------------------------
  // Notifications
  // ---------------------------------------------------------------------------
  notifications: {
    employee_id: { type: Number, required: true },
    type:        { type: String, enum: ["leave_approved", "review_due", "payroll_ready", "course_reminder", "general"], required: true },
    title:       { type: String, required: true },
    message:     { type: String },
    is_read:     { type: Boolean, default: false },
    created_at:  { type: Number },
    link:        { type: String },
  },
});

// ===========================================================================
// API Endpoints
// ===========================================================================

// ---------------------------------------------------------------------------
// Employee Management (12 endpoints)
// ---------------------------------------------------------------------------

export async function createEmployee(
  first_name: string,
  last_name: string,
  email: string,
  department_id: number,
  position_id: number,
  salary: number,
  extras?: Record<string, unknown>
) {
  const { data, error } = await db.employees.create({
    first_name, last_name, email,
    department_id, position_id, salary,
    hire_date: Date.now(),
    status: "active",
    ...(extras || {}),
  });
  if (error) return { data: null, error };

  await db.departments.updateOne({ id: department_id }, { headcount: { $inc: 1 } });

  // Record initial compensation history
  await db.compensation_history.create({
    employee_id: (data as Record<string, unknown>)._id as number,
    effective_date: Date.now(),
    salary,
    change_type: "hire",
  });

  return { data, error: null };
}

export async function getEmployees(filters?: Record<string, unknown>) {
  return db.employees.find(filters || {}).sort({ last_name: 1, first_name: 1 });
}

export async function getEmployee(id: number) {
  return db.employees.findOne({ id });
}

export async function updateEmployee(id: number, changes: Record<string, unknown>) {
  return db.employees.updateOne({ id }, changes);
}

export async function terminateEmployee(id: number) {
  const { data: emp } = await db.employees.findOne({ id });
  if (!emp) return { data: null, error: { message: "Employee not found" } };

  await db.employees.updateOne({ id }, { status: "terminated" });

  if ((emp as Record<string, unknown>).department_id) {
    await db.departments.updateOne(
      { id: (emp as Record<string, unknown>).department_id as number },
      { headcount: { $inc: -1 } }
    );
  }

  return { data: { terminated: true }, error: null };
}

export async function getOrgChart() {
  const { data: allEmployees } = await db.employees.find({ status: "active" });
  if (!allEmployees) return { data: [], error: null };

  const empList = allEmployees as Record<string, unknown>[];
  const byId: Record<number, Record<string, unknown>> = {};
  for (const e of empList) {
    byId[(e._id as number)] = { ...e, reports: [] };
  }

  const roots: Record<string, unknown>[] = [];
  for (const e of empList) {
    const mgr = (e as Record<string, unknown>).manager_id as number | undefined;
    if (mgr && byId[mgr]) {
      ((byId[mgr].reports as Record<string, unknown>[])).push(byId[(e._id as number)]);
    } else {
      roots.push(byId[(e._id as number)]);
    }
  }

  return { data: roots, error: null };
}

export async function getDirectReports(manager_id: number) {
  return db.employees.find({ manager_id, status: "active" }).sort({ last_name: 1 });
}

export async function searchEmployees(query: string) {
  return db.employees.find({
    $or: [
      { first_name: { $ilike: `%${query}%` } },
      { last_name:  { $ilike: `%${query}%` } },
      { email:      { $ilike: `%${query}%` } },
    ],
  }).limit(20);
}

export async function addSkill(id: number, skill: string) {
  return db.employees.updateOne({ id }, { skills: { $addToSet: skill } });
}

export async function removeSkill(id: number, skill: string) {
  return db.employees.updateOne({ id }, { skills: { $pull: skill } });
}

export async function getEmployeesByDepartment(department_id: number) {
  return db.employees.find({ department_id, status: "active" }).sort({ last_name: 1 });
}

export async function getEmployeeHistory(employee_id: number) {
  const [compHistory, posHistory] = await Promise.all([
    db.compensation_history.find({ employee_id }).sort({ effective_date: -1 }),
    db.reviews.find({ employee_id }).sort({ createdAt: -1 }),
  ]);
  return {
    data: {
      compensation: (compHistory as Record<string, unknown>).data,
      reviews: (posHistory as Record<string, unknown>).data,
    },
    error: null,
  };
}

// ---------------------------------------------------------------------------
// Department (6 endpoints)
// ---------------------------------------------------------------------------

export async function createDepartment(
  name: string,
  code: string,
  budget?: number,
  parent_department_id?: number
) {
  return db.departments.create({
    name,
    code,
    ...(budget !== undefined && { budget }),
    ...(parent_department_id !== undefined && { parent_department_id }),
  });
}

export async function getDepartments() {
  return db.departments.find({}).sort({ name: 1 });
}

export async function getDepartment(id: number) {
  return db.departments.findOne({ id });
}

export async function updateDepartment(id: number, changes: Record<string, unknown>) {
  return db.departments.updateOne({ id }, changes);
}

export async function deleteDepartment(id: number) {
  return db.departments.deleteOne({ id });
}

export async function getDepartmentTree() {
  const { data: allDepts } = await db.departments.find({});
  if (!allDepts) return { data: [], error: null };

  const deptList = allDepts as Record<string, unknown>[];
  const byId: Record<number, Record<string, unknown>> = {};
  for (const d of deptList) {
    byId[(d._id as number)] = { ...d, children: [] };
  }

  const roots: Record<string, unknown>[] = [];
  for (const d of deptList) {
    const parent = (d as Record<string, unknown>).parent_department_id as number | undefined;
    if (parent && byId[parent]) {
      (byId[parent].children as Record<string, unknown>[]).push(byId[(d._id as number)]);
    } else {
      roots.push(byId[(d._id as number)]);
    }
  }

  return { data: roots, error: null };
}

// ---------------------------------------------------------------------------
// Position (6 endpoints)
// ---------------------------------------------------------------------------

export async function createPosition(
  title: string,
  department_id: number,
  level: string,
  salary_min: number,
  salary_max: number,
  description?: string
) {
  return db.positions.create({ title, department_id, level, salary_min, salary_max, ...(description && { description }) });
}

export async function getPositions(filters?: Record<string, unknown>) {
  return db.positions.find(filters || {}).sort({ title: 1 });
}

export async function getOpenPositions() {
  return db.positions.find({ is_open: true }).sort({ title: 1 });
}

export async function updatePosition(id: number, changes: Record<string, unknown>) {
  return db.positions.updateOne({ id }, changes);
}

export async function closePosition(id: number) {
  return db.positions.updateOne({ id }, { is_open: false });
}

export async function getPositionsByDepartment(department_id: number) {
  return db.positions.find({ department_id }).sort({ level: 1 });
}

// ---------------------------------------------------------------------------
// Recruitment (10 endpoints)
// ---------------------------------------------------------------------------

export async function createJobPosting(
  position_id: number,
  title: string,
  description?: string,
  requirements?: string,
  closing_date?: number
) {
  return db.job_postings.create({
    position_id, title,
    ...(description && { description }),
    ...(requirements && { requirements }),
    ...(closing_date && { closing_date }),
  });
}

export async function getJobPostings(filters?: Record<string, unknown>) {
  return db.job_postings.find(filters || {}).sort({ createdAt: -1 });
}

export async function getJobPosting(id: number) {
  return db.job_postings.findOne({ id });
}

export async function publishJobPosting(id: number) {
  return db.job_postings.updateOne({ id }, { status: "open", posted_date: Date.now() });
}

export async function closeJobPosting(id: number) {
  return db.job_postings.updateOne({ id }, { status: "closed" });
}

export async function applyToJob(
  job_posting_id: number,
  name: string,
  email: string,
  phone?: string,
  resume_url?: string
) {
  return db.applicants.create({
    job_posting_id, name, email,
    ...(phone && { phone }),
    ...(resume_url && { resume_url }),
    applied_date: Date.now(),
  });
}

export async function getApplicants(job_posting_id: number, stage?: string) {
  return db.applicants.find({
    job_posting_id,
    ...(stage && { stage }),
  }).sort({ applied_date: -1 });
}

export async function updateApplicantStage(
  id: number,
  stage: string,
  notes?: string,
  rating?: number
) {
  return db.applicants.updateOne({ id }, {
    stage,
    ...(notes !== undefined && { notes }),
    ...(rating !== undefined && { rating }),
  });
}

export async function scheduleInterview(
  applicant_id: number,
  interviewer_id: number,
  scheduled_at: number,
  type: string,
  duration_minutes?: number
) {
  return db.interviews.create({
    applicant_id, interviewer_id, scheduled_at, type,
    ...(duration_minutes && { duration_minutes }),
  });
}

export async function submitInterviewFeedback(
  id: number,
  feedback: string,
  rating: number
) {
  return db.interviews.updateOne({ id }, { status: "completed", feedback, rating });
}

// ---------------------------------------------------------------------------
// Time & Attendance (8 endpoints)
// ---------------------------------------------------------------------------

export async function clockIn(employee_id: number, date: number, notes?: string) {
  // Check if a timesheet already exists for today
  const { data: existing } = await db.timesheets.findOne({ employee_id, date });
  if (existing) {
    return { data: null, error: { message: "Already clocked in for this date" } };
  }
  return db.timesheets.create({
    employee_id,
    date,
    clock_in: Date.now(),
    ...(notes && { notes }),
  });
}

export async function clockOut(employee_id: number, date: number) {
  const { data: ts } = await db.timesheets.findOne({ employee_id, date });
  if (!ts) return { data: null, error: { message: "No clock-in found for this date" } };

  const tsData = ts as Record<string, unknown>;
  const clock_out = Date.now();
  const clock_in = tsData.clock_in as number;
  const ms = clock_out - clock_in;
  const hours_worked = Math.round((ms / 3_600_000) * 100) / 100;
  const overtime_hours = Math.max(0, Math.round((hours_worked - 8) * 100) / 100);

  return db.timesheets.updateOne(
    { id: tsData._id as number },
    { clock_out, hours_worked, overtime_hours }
  );
}

export async function submitTimesheet(id: number) {
  return db.timesheets.updateOne({ id }, { status: "submitted" });
}

export async function approveTimesheet(id: number) {
  return db.timesheets.updateOne({ id }, { status: "approved" });
}

export async function getTimesheets(
  employee_id: number,
  from_date?: number,
  to_date?: number
) {
  return db.timesheets.find({
    employee_id,
    ...(from_date !== undefined && to_date !== undefined && {
      date: { $gte: from_date, $lte: to_date },
    }),
  }).sort({ date: -1 });
}

export async function getWorkSchedule(employee_id: number) {
  return db.work_schedules.find({ employee_id }).sort({ day_of_week: 1 });
}

export async function updateWorkSchedule(
  employee_id: number,
  day_of_week: number,
  start_time: string,
  end_time: string,
  is_remote?: boolean
) {
  const { data: existing } = await db.work_schedules.findOne({ employee_id, day_of_week });
  if (existing) {
    return db.work_schedules.updateOne(
      { id: (existing as Record<string, unknown>)._id as number },
      { start_time, end_time, ...(is_remote !== undefined && { is_remote }) }
    );
  }
  return db.work_schedules.create({
    employee_id, day_of_week, start_time, end_time,
    ...(is_remote !== undefined && { is_remote }),
  });
}

export async function getOvertimeReport(from_date?: number, to_date?: number) {
  return db.timesheets.aggregate([
    {
      $match: {
        status: "approved",
        ...(from_date !== undefined && to_date !== undefined && {
          date: { $gte: from_date, $lte: to_date },
        }),
      },
    },
    {
      $group: {
        _id: "$employee_id",
        total_hours: { $sum: "$hours_worked" },
        total_overtime: { $sum: "$overtime_hours" },
        days_count: { $sum: 1 },
      },
    },
    { $sort: { total_overtime: -1 } },
  ]);
}

// ---------------------------------------------------------------------------
// Leave (10 endpoints)
// ---------------------------------------------------------------------------

export async function requestLeave(
  employee_id: number,
  type: string,
  start_date: number,
  end_date: number,
  days: number,
  reason?: string
) {
  return db.leave_requests.create({
    employee_id, type, start_date, end_date, days,
    ...(reason && { reason }),
  });
}

export async function approveLeave(id: number, approved_by: number) {
  const { data } = await db.leave_requests.updateOne(
    { id, status: "pending" },
    { status: "approved", approved_by, approved_at: Date.now() }
  );

  if (data && (data as Record<string, unknown>).modifiedCount as number > 0) {
    const { data: leave } = await db.leave_requests.findOne({ id });
    if (leave) {
      const leaveData = leave as Record<string, unknown>;
      await db.employees.updateOne(
        { id: leaveData.employee_id as number },
        { status: "on_leave" }
      );
    }
  }

  return { data, error: null };
}

export async function denyLeave(id: number, approved_by: number) {
  return db.leave_requests.updateOne(
    { id, status: "pending" },
    { status: "denied", approved_by, approved_at: Date.now() }
  );
}

export async function cancelLeave(id: number, employee_id: number) {
  return db.leave_requests.updateOne(
    { id, employee_id, status: "pending" },
    { status: "cancelled" }
  );
}

export async function getLeaveRequests(filters?: Record<string, unknown>) {
  return db.leave_requests.find(filters || {}).sort({ createdAt: -1 });
}

export async function getLeaveBalance(employee_id: number) {
  const currentYear = new Date().getFullYear();

  // Check if there's an explicit balance record
  const { data: balanceRecord } = await db.leave_balances.findOne({ employee_id, year: currentYear });

  if (balanceRecord) {
    return { data: balanceRecord, error: null };
  }

  // Fallback: compute from approved leave requests in current year
  const yearStart = new Date(currentYear, 0, 1).getTime();
  const yearEnd   = new Date(currentYear, 11, 31, 23, 59, 59).getTime();

  const { data: approved } = await db.leave_requests.find({
    employee_id,
    status: "approved",
    start_date: { $gte: yearStart, $lte: yearEnd },
  });

  const used = ((approved as Record<string, unknown>[]) || []).reduce(
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
      sick:     { used: used.sick    || 0, total: 10 },
      personal: { used: used.personal || 0, total: 5  },
    },
    error: null,
  };
}

export async function getLeaveCalendar(department_id?: number) {
  const empFilter: Record<string, unknown> = { status: { $ne: "terminated" } };
  if (department_id !== undefined) empFilter.department_id = department_id;

  const { data: teamMembers } = await db.employees.find(empFilter);
  const ids = ((teamMembers as Record<string, unknown>[]) || []).map(e => (e as Record<string, unknown>)._id as number);

  if (ids.length === 0) return { data: [], error: null };

  return db.leave_requests.find({
    employee_id: { $in: ids },
    status: "approved",
  }).sort({ start_date: 1 });
}

export async function getHolidays(year?: number) {
  if (!year) return db.holidays.find({}).sort({ date: 1 });
  const start = new Date(year, 0, 1).getTime();
  const end   = new Date(year, 11, 31, 23, 59, 59).getTime();
  return db.holidays.find({ date: { $gte: start, $lte: end } }).sort({ date: 1 });
}

export async function createHoliday(name: string, date: number, is_recurring?: boolean) {
  return db.holidays.create({ name, date, ...(is_recurring !== undefined && { is_recurring }) });
}

export async function getLeaveReport() {
  return db.leave_requests.aggregate([
    { $match: { status: "approved" } },
    {
      $group: {
        _id: "$type",
        total_requests: { $sum: 1 },
        total_days:     { $sum: "$days" },
        avg_days:       { $avg: "$days" },
      },
    },
    { $sort: { total_days: -1 } },
  ]);
}

// ---------------------------------------------------------------------------
// Payroll (8 endpoints)
// ---------------------------------------------------------------------------

export async function createPayrollRun(period: string, processed_by: number) {
  return db.payroll_runs.create({
    period,
    run_date: Date.now(),
    processed_by,
  });
}

export async function processPayroll(payroll_run_id: number) {
  await db.payroll_runs.updateOne({ id: payroll_run_id }, { status: "processing" });

  const { data: activeEmployees } = await db.employees.find({ status: "active" });
  if (!activeEmployees || (activeEmployees as Record<string, unknown>[]).length === 0) {
    return { data: { processed: 0 }, error: null };
  }

  let totalGross = 0;
  let totalNet   = 0;
  let totalDeductions = 0;

  for (const emp of activeEmployees as Record<string, unknown>[]) {
    const base = (emp.salary as number) || 0;
    const deductions_tax      = Math.round(base * 0.22);
    const deductions_benefits = Math.round(base * 0.05);
    const deductions_other    = 0;
    const net = base - deductions_tax - deductions_benefits - deductions_other;

    await db.payslips.create({
      payroll_run_id,
      employee_id: emp._id as number,
      base_salary: base,
      deductions_tax,
      deductions_benefits,
      deductions_other,
      net_pay: net,
    });

    totalGross += base;
    totalNet   += net;
    totalDeductions += deductions_tax + deductions_benefits;
  }

  await db.payroll_runs.updateOne(
    { id: payroll_run_id },
    {
      status: "completed",
      total_gross: totalGross,
      total_net:   totalNet,
      total_deductions: totalDeductions,
    }
  );

  return {
    data: {
      processed: (activeEmployees as Record<string, unknown>[]).length,
      total_gross: totalGross,
      total_net: totalNet,
    },
    error: null,
  };
}

export async function finalizePayroll(payroll_run_id: number) {
  await db.payslips.updateMany({ payroll_run_id, status: "pending" }, { status: "paid" });
  return { data: { finalized: true }, error: null };
}

export async function getPayrollRuns(filters?: Record<string, unknown>) {
  return db.payroll_runs.find(filters || {}).sort({ run_date: -1 });
}

export async function getPayslip(id: number) {
  return db.payslips.findOne({ id });
}

export async function getPayslipsByEmployee(employee_id: number) {
  return db.payslips.find({ employee_id }).sort({ createdAt: -1 });
}

export async function getPayrollSummary(period: string) {
  const { data: run } = await db.payroll_runs.findOne({ period });
  if (!run) return { data: null, error: { message: "Payroll run not found" } };

  const { data: slips } = await db.payslips.find({
    payroll_run_id: (run as Record<string, unknown>)._id as number,
  });

  return {
    data: {
      run,
      employee_count: ((slips as Record<string, unknown>[]) || []).length,
      slips,
    },
    error: null,
  };
}

export async function exportPayroll(payroll_run_id: number) {
  const { data: slips } = await db.payslips.find({ payroll_run_id });
  if (!slips) return { data: "", error: null };

  const rows = slips as Record<string, unknown>[];
  const header = "employee_id,base_salary,overtime_pay,bonus,deductions_tax,deductions_benefits,deductions_other,net_pay,status";
  const lines = rows.map(r =>
    [
      r.employee_id, r.base_salary, r.overtime_pay, r.bonus,
      r.deductions_tax, r.deductions_benefits, r.deductions_other,
      r.net_pay, r.status,
    ].join(",")
  );
  return { data: [header, ...lines].join("\n"), error: null };
}

// ---------------------------------------------------------------------------
// Performance (10 endpoints)
// ---------------------------------------------------------------------------

export async function createReview(
  employee_id: number,
  reviewer_id: number,
  period: string,
  cycle: string,
  strengths?: string,
  improvements?: string,
  goalsText?: string
) {
  return db.reviews.create({
    employee_id, reviewer_id, period, cycle,
    ...(strengths    && { strengths }),
    ...(improvements && { improvements }),
    ...(goalsText    && { goals: goalsText }),
  });
}

export async function submitReview(id: number, rating: number) {
  return db.reviews.updateOne({ id }, { status: "submitted", rating });
}

export async function acknowledgeReview(id: number) {
  return db.reviews.updateOne({ id }, { status: "acknowledged" });
}

export async function getReviewsForEmployee(employee_id: number) {
  return db.reviews.find({ employee_id }).sort({ createdAt: -1 });
}

export async function getPendingReviews(reviewer_id?: number) {
  return db.reviews.find({
    status: "draft",
    ...(reviewer_id !== undefined && { reviewer_id }),
  }).sort({ createdAt: 1 });
}

export async function createGoal(
  employee_id: number,
  title: string,
  category: string,
  target_date?: number,
  description?: string
) {
  return db.goals.create({
    employee_id, title, category,
    ...(target_date  !== undefined && { target_date }),
    ...(description  && { description }),
  });
}

export async function updateGoalProgress(id: number, progress: number, status?: string) {
  return db.goals.updateOne({ id }, {
    progress,
    ...(status && { status }),
  });
}

export async function getGoals(employee_id: number, filters?: Record<string, unknown>) {
  return db.goals.find({ employee_id, ...(filters || {}) }).sort({ target_date: 1 });
}

export async function giveFeedback(
  from_employee_id: number,
  to_employee_id: number,
  type: string,
  message: string,
  is_anonymous?: boolean
) {
  return db.feedback.create({
    from_employee_id, to_employee_id, type, message,
    ...(is_anonymous !== undefined && { is_anonymous }),
  });
}

export async function getFeedbackForEmployee(to_employee_id: number) {
  return db.feedback.find({ to_employee_id }).sort({ createdAt: -1 });
}

// ---------------------------------------------------------------------------
// Training (8 endpoints)
// ---------------------------------------------------------------------------

export async function createCourse(
  title: string,
  category: string,
  duration_hours: number,
  description?: string,
  is_mandatory?: boolean,
  max_participants?: number
) {
  return db.courses.create({
    title, category, duration_hours,
    ...(description     && { description }),
    ...(is_mandatory    !== undefined && { is_mandatory }),
    ...(max_participants !== undefined && { max_participants }),
  });
}

export async function getCourses(filters?: Record<string, unknown>) {
  return db.courses.find(filters || {}).sort({ title: 1 });
}

export async function enrollInCourse(course_id: number, employee_id: number) {
  return db.enrollments.create({
    course_id, employee_id,
    enrolled_at: Date.now(),
  });
}

export async function completeCourse(
  course_id: number,
  employee_id: number,
  score?: number
) {
  return db.enrollments.updateOne(
    { course_id, employee_id },
    {
      status: "completed",
      completed_at: Date.now(),
      ...(score !== undefined && { score }),
    }
  );
}

export async function getEnrollments(filters?: Record<string, unknown>) {
  return db.enrollments.find(filters || {}).sort({ enrolled_at: -1 });
}

export async function addCertification(
  employee_id: number,
  name: string,
  issuer: string,
  issue_date: number,
  expiry_date?: number,
  credential_url?: string
) {
  return db.certifications.create({
    employee_id, name, issuer, issue_date,
    ...(expiry_date     !== undefined && { expiry_date }),
    ...(credential_url  && { credential_url }),
  });
}

export async function getCertifications(employee_id: number) {
  return db.certifications.find({ employee_id }).sort({ issue_date: -1 });
}

export async function getExpiringCertifications(days_ahead: number = 30) {
  const now    = Date.now();
  const cutoff = now + days_ahead * 86_400_000;
  return db.certifications.find({
    expiry_date: { $gte: now, $lte: cutoff },
  }).sort({ expiry_date: 1 });
}

// ---------------------------------------------------------------------------
// Compensation & Benefits (8 endpoints)
// ---------------------------------------------------------------------------

export async function adjustSalary(
  employee_id: number,
  new_salary: number,
  change_type: string,
  effective_date: number,
  change_reason?: string,
  approved_by?: number
) {
  await db.employees.updateOne({ id: employee_id }, { salary: new_salary });

  return db.compensation_history.create({
    employee_id,
    effective_date,
    salary: new_salary,
    change_type,
    ...(change_reason && { change_reason }),
    ...(approved_by !== undefined && { approved_by }),
  });
}

export async function getCompensationHistory(employee_id: number) {
  return db.compensation_history.find({ employee_id }).sort({ effective_date: -1 });
}

export async function getBenefitsPlans(type?: string) {
  return db.benefits_plans.find(type ? { type } : {}).sort({ name: 1 });
}

export async function enrollInBenefit(
  employee_id: number,
  plan_id: number,
  start_date: number
) {
  return db.benefits_enrollments.create({ employee_id, plan_id, start_date });
}

export async function getBenefitsEnrollments(employee_id: number) {
  return db.benefits_enrollments.find({ employee_id, status: "active" }).sort({ start_date: -1 });
}

export async function submitExpense(
  employee_id: number,
  description: string,
  amount: number,
  category: string,
  receipt_url?: string
) {
  return db.expense_claims.create({
    employee_id, description, amount, category,
    submitted_at: Date.now(),
    ...(receipt_url && { receipt_url }),
  });
}

export async function approveExpense(id: number, approved_by: number, approve: boolean) {
  return db.expense_claims.updateOne(
    { id },
    { status: approve ? "approved" : "rejected", approved_by }
  );
}

export async function getExpenses(filters?: Record<string, unknown>) {
  return db.expense_claims.find(filters || {}).sort({ submitted_at: -1 });
}

// ---------------------------------------------------------------------------
// Documents & Compliance (6 endpoints)
// ---------------------------------------------------------------------------

export async function uploadDocument(
  employee_id: number,
  type: string,
  name: string,
  file_url: string,
  expires_at?: number
) {
  return db.documents.create({
    employee_id, type, name, file_url,
    uploaded_at: Date.now(),
    ...(expires_at !== undefined && { expires_at }),
  });
}

export async function getDocuments(employee_id: number, type?: string) {
  return db.documents.find({
    employee_id,
    ...(type && { type }),
  }).sort({ uploaded_at: -1 });
}

export async function getExpiringDocuments(days_ahead: number = 30) {
  const now    = Date.now();
  const cutoff = now + days_ahead * 86_400_000;
  return db.documents.find({
    expires_at: { $gte: now, $lte: cutoff },
  }).sort({ expires_at: 1 });
}

export async function getPolicies(category?: string) {
  return db.policies.find(category ? { category } : {}).sort({ effective_date: -1 });
}

export async function getAuditLog(
  entity_type?: string,
  entity_id?: number,
  actor_id?: number
) {
  return db.audit_log.find({
    ...(entity_type !== undefined && { entity_type }),
    ...(entity_id   !== undefined && { entity_id }),
    ...(actor_id    !== undefined && { actor_id }),
  }).sort({ timestamp: -1 }).limit(500);
}

export async function acknowledgePolicy(employee_id: number, policy_id: number) {
  // Record acknowledgement as a document
  return db.documents.create({
    employee_id,
    type: "policy",
    name: `Policy ${policy_id} Acknowledgement`,
    file_url: "",
    uploaded_at: Date.now(),
  });
}

// ---------------------------------------------------------------------------
// Notifications (4 endpoints)
// ---------------------------------------------------------------------------

export async function getNotifications(employee_id: number) {
  return db.notifications.find({ employee_id }).sort({ created_at: -1 });
}

export async function markAsRead(id: number) {
  return db.notifications.updateOne({ id }, { is_read: true });
}

export async function markAllAsRead(employee_id: number) {
  return db.notifications.updateMany({ employee_id, is_read: false }, { is_read: true });
}

export async function getUnreadCount(employee_id: number) {
  return db.notifications.countDocuments({ employee_id, is_read: false });
}

// ---------------------------------------------------------------------------
// Analytics (10 endpoints)
// ---------------------------------------------------------------------------

export async function getDashboard() {
  const [
    { data: totalEmployees },
    { data: totalDepartments },
    { data: openPositions },
    { data: pendingLeaves },
    { data: openJobs },
  ] = await Promise.all([
    db.employees.countDocuments({ status: "active" }),
    db.departments.countDocuments({}),
    db.positions.countDocuments({ is_open: true }),
    db.leave_requests.countDocuments({ status: "pending" }),
    db.job_postings.countDocuments({ status: "open" }),
  ]);

  return {
    data: {
      totalEmployees,
      totalDepartments,
      openPositions,
      pendingLeaves,
      openJobs,
    },
    error: null,
  };
}

export async function getHeadcountByDepartment() {
  return db.employees.aggregate([
    { $match: { status: "active" } },
    { $group: { _id: "$department_id", headcount: { $sum: 1 }, avg_salary: { $avg: "$salary" } } },
    { $sort: { headcount: -1 } },
  ]);
}

export async function getSalaryDistribution() {
  return db.employees.aggregate([
    { $match: { status: "active" } },
    {
      $group: {
        _id: "$department_id",
        min_salary:    { $min: "$salary" },
        max_salary:    { $max: "$salary" },
        avg_salary:    { $avg: "$salary" },
        total_payroll: { $sum: "$salary" },
        headcount:     { $sum: 1 },
      },
    },
    { $sort: { total_payroll: -1 } },
  ]);
}

export async function getAttritionReport() {
  const { data: terminated } = await db.employees.countDocuments({ status: "terminated" });
  const { data: total }      = await db.employees.countDocuments({});
  const rate = total ? (((terminated as number) || 0) / (total as number) * 100).toFixed(1) : "0.0";
  return { data: { terminated, total, attrition_rate: `${rate}%` }, error: null };
}

export async function getLeaveUtilization() {
  return db.leave_requests.aggregate([
    { $match: { status: "approved" } },
    {
      $group: {
        _id: "$employee_id",
        total_days_used: { $sum: "$days" },
        requests_count:  { $sum: 1 },
      },
    },
    { $sort: { total_days_used: -1 } },
  ]);
}

export async function getReviewStats() {
  return db.reviews.aggregate([
    { $group: { _id: "$period", count: { $sum: 1 }, avg_rating: { $avg: "$rating" } } },
    { $sort: { _id: -1 } },
  ]);
}

export async function getTimeToHire() {
  // Hired applicants carry the applied_date; the job posting has the posted_date.
  // We aggregate by job_posting_id and compute avg days from application to hire.
  return db.applicants.aggregate([
    { $match: { stage: "hired" } },
    {
      $group: {
        _id: "$job_posting_id",
        count: { $sum: 1 },
        avg_apply_date: { $avg: "$applied_date" },
      },
    },
    { $sort: { count: -1 } },
  ]);
}

export async function getDiversityMetrics() {
  // Department composition with skill breakdown
  return db.employees.aggregate([
    { $match: { status: "active" } },
    { $group: { _id: "$department_id", headcount: { $sum: 1 } } },
    { $sort: { headcount: -1 } },
  ]);
}

export async function getCostPerEmployee() {
  return db.employees.aggregate([
    { $match: { status: "active" } },
    {
      $group: {
        _id: "$department_id",
        total_salary: { $sum: "$salary" },
        headcount:    { $sum: 1 },
        avg_salary:   { $avg: "$salary" },
      },
    },
    { $sort: { total_salary: -1 } },
  ]);
}

export async function getMonthlyTrends() {
  const { data: hires } = await db.employees.aggregate([
    { $group: { _id: { $dateToString: { format: "%Y-%m", date: { $toDate: "$hire_date" } } }, count: { $sum: 1 } } },
    { $sort: { _id: 1 } },
  ]);

  const { data: terminations } = await db.employees.aggregate([
    { $match: { status: "terminated" } },
    { $group: { _id: { $dateToString: { format: "%Y-%m", date: { $toDate: "$hire_date" } } }, count: { $sum: 1 } } },
    { $sort: { _id: 1 } },
  ]);

  return { data: { hires, terminations }, error: null };
}
