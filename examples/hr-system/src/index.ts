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

import { createDb, schema } from "@zeroship/db";

const db = createDb({
  // ---------------------------------------------------------------------------
  // Core
  // ---------------------------------------------------------------------------
  departments: {
    name:               { type: String, required: true },
    code:               { type: String, required: true },
    managerId:          { type: Number },
    budget:             { type: Number, default: 0 },
    headcount:          { type: Number, default: 0 },
    parentDepartmentId: { type: Number },
  },

  positions: {
    title:        { type: String, required: true },
    departmentId: { type: Number, required: true },
    level:        { type: String, enum: ["junior", "mid", "senior", "lead", "director", "vp", "c-level"] },
    salaryMin:    { type: Number },
    salaryMax:    { type: Number },
    isOpen:       { type: Boolean, default: true },
    description:  { type: String },
  },

  employees: schema({
    firstName:             { type: String, required: true },
    lastName:              { type: String, required: true },
    email:                 { type: String, required: true },
    phone:                 { type: String },
    departmentId:          { type: Number },
    positionId:            { type: Number },
    managerId:             { type: Number },
    hireDate:              { type: Number },
    salary:                { type: Number },
    status:                { type: String, enum: ["active", "on_leave", "terminated"], default: "active" },
    skills:                { type: [String] },
    avatarUrl:             { type: String },
    emergencyContactName:  { type: String },
    emergencyContactPhone: { type: String },
    address:               { type: String },
    dateOfBirth:           { type: Number },
  }).softDelete(),

  // ---------------------------------------------------------------------------
  // Recruitment
  // ---------------------------------------------------------------------------
  jobPostings: {
    positionId:  { type: Number, required: true },
    title:       { type: String, required: true },
    description: { type: String },
    requirements:{ type: String },
    status:      { type: String, enum: ["draft", "open", "closed"], default: "draft" },
    postedDate:  { type: Number },
    closingDate: { type: Number },
  },

  applicants: {
    jobPostingId: { type: Number, required: true },
    name:         { type: String, required: true },
    email:        { type: String, required: true },
    phone:        { type: String },
    resumeUrl:    { type: String },
    stage:        { type: String, enum: ["applied", "screening", "interview", "offer", "hired", "rejected"], default: "applied" },
    rating:       { type: Number },
    notes:        { type: String },
    appliedDate:  { type: Number },
  },

  interviews: {
    applicantId:     { type: Number, required: true },
    interviewerId:   { type: Number, required: true },
    scheduledAt:     { type: Number, required: true },
    durationMinutes: { type: Number, default: 60 },
    type:            { type: String, enum: ["phone", "video", "onsite"], default: "video" },
    status:          { type: String, enum: ["scheduled", "completed", "cancelled"], default: "scheduled" },
    feedback:        { type: String },
    rating:          { type: Number },
  },

  // ---------------------------------------------------------------------------
  // Time & Attendance
  // ---------------------------------------------------------------------------
  timesheets: {
    employeeId:    { type: Number, required: true },
    date:          { type: Number, required: true },
    clockIn:       { type: Number },
    clockOut:      { type: Number },
    hoursWorked:   { type: Number, default: 0 },
    overtimeHours: { type: Number, default: 0 },
    status:        { type: String, enum: ["draft", "submitted", "approved"], default: "draft" },
    notes:         { type: String },
  },

  workSchedules: {
    employeeId: { type: Number, required: true },
    dayOfWeek:  { type: Number, required: true }, // 0=Sun, 6=Sat
    startTime:  { type: String, required: true },
    endTime:    { type: String, required: true },
    isRemote:   { type: Boolean, default: false },
  },

  // ---------------------------------------------------------------------------
  // Leave
  // ---------------------------------------------------------------------------
  leaveRequests: {
    employeeId: { type: Number, required: true },
    type:       { type: String, required: true, enum: ["vacation", "sick", "personal", "parental", "bereavement"] },
    startDate:  { type: Number, required: true },
    endDate:    { type: Number, required: true },
    days:       { type: Number, required: true },
    reason:     { type: String },
    status:     { type: String, enum: ["pending", "approved", "denied", "cancelled"], default: "pending" },
    approvedBy: { type: Number },
    approvedAt: { type: Number },
  },

  leaveBalances: {
    employeeId:    { type: Number, required: true },
    year:          { type: Number, required: true },
    vacationTotal: { type: Number, default: 20 },
    vacationUsed:  { type: Number, default: 0 },
    sickTotal:     { type: Number, default: 10 },
    sickUsed:      { type: Number, default: 0 },
    personalTotal: { type: Number, default: 5 },
    personalUsed:  { type: Number, default: 0 },
  },

  holidays: {
    name:        { type: String, required: true },
    date:        { type: Number, required: true },
    isRecurring: { type: Boolean, default: false },
  },

  // ---------------------------------------------------------------------------
  // Payroll
  // ---------------------------------------------------------------------------
  payrollRuns: {
    period:          { type: String, required: true },
    runDate:         { type: Number },
    status:          { type: String, enum: ["draft", "processing", "completed"], default: "draft" },
    totalGross:      { type: Number, default: 0 },
    totalNet:        { type: Number, default: 0 },
    totalDeductions: { type: Number, default: 0 },
    processedBy:     { type: Number },
  },

  payslips: {
    payrollRunId:       { type: Number, required: true },
    employeeId:         { type: Number, required: true },
    baseSalary:         { type: Number, required: true },
    overtimePay:        { type: Number, default: 0 },
    bonus:              { type: Number, default: 0 },
    deductionsTax:      { type: Number, default: 0 },
    deductionsBenefits: { type: Number, default: 0 },
    deductionsOther:    { type: Number, default: 0 },
    netPay:             { type: Number, required: true },
    status:             { type: String, enum: ["pending", "paid"], default: "pending" },
  },

  // ---------------------------------------------------------------------------
  // Performance
  // ---------------------------------------------------------------------------
  reviews: {
    employeeId:  { type: Number, required: true },
    reviewerId:  { type: Number, required: true },
    period:      { type: String, required: true },
    cycle:       { type: String, enum: ["quarterly", "annual"], default: "annual" },
    rating:      { type: Number, min: 1, max: 5 },
    strengths:   { type: String },
    improvements:{ type: String },
    goals:       { type: String },
    status:      { type: String, enum: ["draft", "submitted", "acknowledged"], default: "draft" },
  },

  goals: {
    employeeId:  { type: Number, required: true },
    title:       { type: String, required: true },
    description: { type: String },
    targetDate:  { type: Number },
    status:      { type: String, enum: ["active", "completed", "cancelled"], default: "active" },
    progress:    { type: Number, default: 0, min: 0, max: 100 },
    category:    { type: String, enum: ["performance", "development", "project"], default: "performance" },
  },

  feedback: {
    fromEmployeeId: { type: Number, required: true },
    toEmployeeId:   { type: Number, required: true },
    type:           { type: String, enum: ["praise", "constructive"], required: true },
    message:        { type: String, required: true },
    isAnonymous:    { type: Boolean, default: false },
  },

  // ---------------------------------------------------------------------------
  // Training
  // ---------------------------------------------------------------------------
  courses: {
    title:           { type: String, required: true },
    description:     { type: String },
    category:        { type: String },
    durationHours:   { type: Number },
    isMandatory:     { type: Boolean, default: false },
    maxParticipants: { type: Number },
  },

  enrollments: {
    courseId:     { type: Number, required: true },
    employeeId:  { type: Number, required: true },
    status:      { type: String, enum: ["enrolled", "in_progress", "completed", "dropped"], default: "enrolled" },
    enrolledAt:  { type: Number },
    completedAt: { type: Number },
    score:       { type: Number },
  },

  certifications: {
    employeeId:    { type: Number, required: true },
    name:          { type: String, required: true },
    issuer:        { type: String },
    issueDate:     { type: Number },
    expiryDate:    { type: Number },
    credentialUrl: { type: String },
  },

  // ---------------------------------------------------------------------------
  // Compensation & Benefits
  // ---------------------------------------------------------------------------
  compensationHistory: {
    employeeId:    { type: Number, required: true },
    effectiveDate: { type: Number, required: true },
    salary:        { type: Number, required: true },
    changeType:    { type: String, enum: ["hire", "promotion", "adjustment", "annual"], required: true },
    changeReason:  { type: String },
    approvedBy:    { type: Number },
  },

  benefitsPlans: {
    name:                  { type: String, required: true },
    type:                  { type: String, enum: ["health", "dental", "vision", "life", "retirement"], required: true },
    provider:              { type: String },
    monthlyCostEmployee:   { type: Number, default: 0 },
    monthlyCostEmployer:   { type: Number, default: 0 },
  },

  benefitsEnrollments: {
    employeeId: { type: Number, required: true },
    planId:     { type: Number, required: true },
    startDate:  { type: Number, required: true },
    endDate:    { type: Number },
    status:     { type: String, enum: ["active", "cancelled"], default: "active" },
  },

  expenseClaims: {
    employeeId:  { type: Number, required: true },
    description: { type: String, required: true },
    amount:      { type: Number, required: true },
    category:    { type: String, enum: ["travel", "meals", "equipment", "other"], required: true },
    receiptUrl:  { type: String },
    status:      { type: String, enum: ["submitted", "approved", "rejected", "reimbursed"], default: "submitted" },
    submittedAt: { type: Number },
    approvedBy:  { type: Number },
  },

  // ---------------------------------------------------------------------------
  // Documents & Compliance
  // ---------------------------------------------------------------------------
  documents: {
    employeeId: { type: Number, required: true },
    type:       { type: String, enum: ["contract", "id", "certification", "policy", "other"], required: true },
    name:       { type: String, required: true },
    fileUrl:    { type: String, required: true },
    uploadedAt: { type: Number },
    expiresAt:  { type: Number },
  },

  auditLog: {
    actorId:    { type: Number, required: true },
    action:     { type: String, enum: ["create", "update", "delete"], required: true },
    entityType: { type: String, required: true },
    entityId:   { type: Number, required: true },
    changesJson:{ type: String },
    timestamp:  { type: Number },
  },

  policies: {
    title:         { type: String, required: true },
    content:       { type: String },
    version:       { type: String },
    effectiveDate: { type: Number },
    category:      { type: String, enum: ["handbook", "conduct", "safety", "privacy"], required: true },
  },

  // ---------------------------------------------------------------------------
  // Notifications
  // ---------------------------------------------------------------------------
  notifications: {
    employeeId: { type: Number, required: true },
    type:       { type: String, enum: ["leave_approved", "review_due", "payroll_ready", "course_reminder", "general"], required: true },
    title:      { type: String, required: true },
    message:    { type: String },
    isRead:     { type: Boolean, default: false },
    link:       { type: String },
  },
});

// ===========================================================================
// API Endpoints
// ===========================================================================

// ---------------------------------------------------------------------------
// Employee Management (12 endpoints)
// ---------------------------------------------------------------------------

export async function createEmployee(
  firstName: string,
  lastName: string,
  email: string,
  departmentId: number,
  positionId: number,
  salary: number,
  extras?: Record<string, unknown>
) {
  const { data, error } = await db.employees.create({
    firstName, lastName, email,
    departmentId, positionId, salary,
    hireDate: Date.now(),
    status: "active",
    ...(extras || {}),
  });
  if (error) return { data: null, error };

  await db.departments.updateOne({ id: departmentId }, { headcount: { $inc: 1 } });

  // Record initial compensation history
  await db.compensationHistory.create({
    employeeId: (data as Record<string, unknown>)._id as number,
    effectiveDate: Date.now(),
    salary,
    changeType: "hire",
  });

  return { data, error: null };
}

export async function getEmployees(filters?: Record<string, unknown>) {
  return db.employees.find(filters || {}).sort({ lastName: 1, firstName: 1 });
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

  if ((emp as Record<string, unknown>).departmentId) {
    await db.departments.updateOne(
      { id: (emp as Record<string, unknown>).departmentId as number },
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
    const mgr = (e as Record<string, unknown>).managerId as number | undefined;
    if (mgr && byId[mgr]) {
      ((byId[mgr].reports as Record<string, unknown>[])).push(byId[(e._id as number)]);
    } else {
      roots.push(byId[(e._id as number)]);
    }
  }

  return { data: roots, error: null };
}

export async function getDirectReports(managerId: number) {
  return db.employees.find({ managerId, status: "active" }).sort({ lastName: 1 });
}

export async function searchEmployees(query: string) {
  return db.employees.find({
    $or: [
      { firstName: { $ilike: `%${query}%` } },
      { lastName:  { $ilike: `%${query}%` } },
      { email:     { $ilike: `%${query}%` } },
    ],
  }).limit(20);
}

export async function addSkill(id: number, skill: string) {
  return db.employees.updateOne({ id }, { skills: { $addToSet: skill } });
}

export async function removeSkill(id: number, skill: string) {
  return db.employees.updateOne({ id }, { skills: { $pull: skill } });
}

export async function getEmployeesByDepartment(departmentId: number) {
  return db.employees.find({ departmentId, status: "active" }).sort({ lastName: 1 });
}

export async function getEmployeeHistory(employeeId: number) {
  const [compHistory, posHistory] = await Promise.all([
    db.compensationHistory.find({ employeeId }).sort({ effectiveDate: -1 }),
    db.reviews.find({ employeeId }).sort({ createdAt: -1 }),
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
  parentDepartmentId?: number
) {
  return db.departments.create({
    name,
    code,
    ...(budget !== undefined && { budget }),
    ...(parentDepartmentId !== undefined && { parentDepartmentId }),
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
    const parent = (d as Record<string, unknown>).parentDepartmentId as number | undefined;
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
  departmentId: number,
  level: string,
  salaryMin: number,
  salaryMax: number,
  description?: string
) {
  return db.positions.create({ title, departmentId, level, salaryMin, salaryMax, ...(description && { description }) });
}

export async function getPositions(filters?: Record<string, unknown>) {
  return db.positions.find(filters || {}).sort({ title: 1 });
}

export async function getOpenPositions() {
  return db.positions.find({ isOpen: true }).sort({ title: 1 });
}

export async function updatePosition(id: number, changes: Record<string, unknown>) {
  return db.positions.updateOne({ id }, changes);
}

export async function closePosition(id: number) {
  return db.positions.updateOne({ id }, { isOpen: false });
}

export async function getPositionsByDepartment(departmentId: number) {
  return db.positions.find({ departmentId }).sort({ level: 1 });
}

// ---------------------------------------------------------------------------
// Recruitment (10 endpoints)
// ---------------------------------------------------------------------------

export async function createJobPosting(
  positionId: number,
  title: string,
  description?: string,
  requirements?: string,
  closingDate?: number
) {
  return db.jobPostings.create({
    positionId, title,
    ...(description && { description }),
    ...(requirements && { requirements }),
    ...(closingDate && { closingDate }),
  });
}

export async function getJobPostings(filters?: Record<string, unknown>) {
  return db.jobPostings.find(filters || {}).sort({ createdAt: -1 });
}

export async function getJobPosting(id: number) {
  return db.jobPostings.findOne({ id });
}

export async function publishJobPosting(id: number) {
  return db.jobPostings.updateOne({ id }, { status: "open", postedDate: Date.now() });
}

export async function closeJobPosting(id: number) {
  return db.jobPostings.updateOne({ id }, { status: "closed" });
}

export async function applyToJob(
  jobPostingId: number,
  name: string,
  email: string,
  phone?: string,
  resumeUrl?: string
) {
  return db.applicants.create({
    jobPostingId, name, email,
    ...(phone && { phone }),
    ...(resumeUrl && { resumeUrl }),
    appliedDate: Date.now(),
  });
}

export async function getApplicants(jobPostingId: number, stage?: string) {
  return db.applicants.find({
    jobPostingId,
    ...(stage && { stage }),
  }).sort({ appliedDate: -1 });
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
  applicantId: number,
  interviewerId: number,
  scheduledAt: number,
  type: string,
  durationMinutes?: number
) {
  return db.interviews.create({
    applicantId, interviewerId, scheduledAt, type,
    ...(durationMinutes && { durationMinutes }),
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

export async function clockIn(employeeId: number, date: number, notes?: string) {
  // Check if a timesheet already exists for today
  const { data: existing } = await db.timesheets.findOne({ employeeId, date });
  if (existing) {
    return { data: null, error: { message: "Already clocked in for this date" } };
  }
  return db.timesheets.create({
    employeeId,
    date,
    clockIn: Date.now(),
    ...(notes && { notes }),
  });
}

export async function clockOut(employeeId: number, date: number) {
  const { data: ts } = await db.timesheets.findOne({ employeeId, date });
  if (!ts) return { data: null, error: { message: "No clock-in found for this date" } };

  const tsData = ts as Record<string, unknown>;
  const clockOutTime = Date.now();
  const clockInTime = tsData.clockIn as number;
  const ms = clockOutTime - clockInTime;
  const hoursWorked = Math.round((ms / 3_600_000) * 100) / 100;
  const overtimeHours = Math.max(0, Math.round((hoursWorked - 8) * 100) / 100);

  return db.timesheets.updateOne(
    { id: tsData._id as number },
    { clockOut: clockOutTime, hoursWorked, overtimeHours }
  );
}

export async function submitTimesheet(id: number) {
  return db.timesheets.updateOne({ id }, { status: "submitted" });
}

export async function approveTimesheet(id: number) {
  return db.timesheets.updateOne({ id }, { status: "approved" });
}

export async function getTimesheets(
  employeeId: number,
  fromDate?: number,
  toDate?: number
) {
  return db.timesheets.find({
    employeeId,
    ...(fromDate !== undefined && toDate !== undefined && {
      date: { $gte: fromDate, $lte: toDate },
    }),
  }).sort({ date: -1 });
}

export async function getWorkSchedule(employeeId: number) {
  return db.workSchedules.find({ employeeId }).sort({ dayOfWeek: 1 });
}

export async function updateWorkSchedule(
  employeeId: number,
  dayOfWeek: number,
  startTime: string,
  endTime: string,
  isRemote?: boolean
) {
  const { data: existing } = await db.workSchedules.findOne({ employeeId, dayOfWeek });
  if (existing) {
    return db.workSchedules.updateOne(
      { id: (existing as Record<string, unknown>)._id as number },
      { startTime, endTime, ...(isRemote !== undefined && { isRemote }) }
    );
  }
  return db.workSchedules.create({
    employeeId, dayOfWeek, startTime, endTime,
    ...(isRemote !== undefined && { isRemote }),
  });
}

export async function getOvertimeReport(fromDate?: number, toDate?: number) {
  return db.timesheets.aggregate([
    {
      $match: {
        status: "approved",
        ...(fromDate !== undefined && toDate !== undefined && {
          date: { $gte: fromDate, $lte: toDate },
        }),
      },
    },
    {
      $group: {
        _id: "$employeeId",
        totalHours: { $sum: "$hoursWorked" },
        totalOvertime: { $sum: "$overtimeHours" },
        daysCount: { $sum: 1 },
      },
    },
    { $sort: { totalOvertime: -1 } },
  ]);
}

// ---------------------------------------------------------------------------
// Leave (10 endpoints)
// ---------------------------------------------------------------------------

export async function requestLeave(
  employeeId: number,
  type: string,
  startDate: number,
  endDate: number,
  days: number,
  reason?: string
) {
  return db.leaveRequests.create({
    employeeId, type, startDate, endDate, days,
    ...(reason && { reason }),
  });
}

export async function approveLeave(id: number, approvedBy: number) {
  const { data } = await db.leaveRequests.updateOne(
    { id, status: "pending" },
    { status: "approved", approvedBy, approvedAt: Date.now() }
  );

  if (data && (data as Record<string, unknown>).modifiedCount as number > 0) {
    const { data: leave } = await db.leaveRequests.findOne({ id });
    if (leave) {
      const leaveData = leave as Record<string, unknown>;
      await db.employees.updateOne(
        { id: leaveData.employeeId as number },
        { status: "on_leave" }
      );
    }
  }

  return { data, error: null };
}

export async function denyLeave(id: number, approvedBy: number) {
  return db.leaveRequests.updateOne(
    { id, status: "pending" },
    { status: "denied", approvedBy, approvedAt: Date.now() }
  );
}

export async function cancelLeave(id: number, employeeId: number) {
  return db.leaveRequests.updateOne(
    { id, employeeId, status: "pending" },
    { status: "cancelled" }
  );
}

export async function getLeaveRequests(filters?: Record<string, unknown>) {
  return db.leaveRequests.find(filters || {}).sort({ createdAt: -1 });
}

export async function getLeaveBalance(employeeId: number) {
  const currentYear = new Date().getFullYear();

  // Check if there's an explicit balance record
  const { data: balanceRecord } = await db.leaveBalances.findOne({ employeeId, year: currentYear });

  if (balanceRecord) {
    return { data: balanceRecord, error: null };
  }

  // Fallback: compute from approved leave requests in current year
  const yearStart = new Date(currentYear, 0, 1).getTime();
  const yearEnd   = new Date(currentYear, 11, 31, 23, 59, 59).getTime();

  const { data: approved } = await db.leaveRequests.find({
    employeeId,
    status: "approved",
    startDate: { $gte: yearStart, $lte: yearEnd },
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

export async function getLeaveCalendar(departmentId?: number) {
  const empFilter: Record<string, unknown> = { status: { $ne: "terminated" } };
  if (departmentId !== undefined) empFilter.departmentId = departmentId;

  const { data: teamMembers } = await db.employees.find(empFilter);
  const ids = ((teamMembers as Record<string, unknown>[]) || []).map(e => (e as Record<string, unknown>)._id as number);

  if (ids.length === 0) return { data: [], error: null };

  return db.leaveRequests.find({
    employeeId: { $in: ids },
    status: "approved",
  }).sort({ startDate: 1 });
}

export async function getHolidays(year?: number) {
  if (!year) return db.holidays.find({}).sort({ date: 1 });
  const start = new Date(year, 0, 1).getTime();
  const end   = new Date(year, 11, 31, 23, 59, 59).getTime();
  return db.holidays.find({ date: { $gte: start, $lte: end } }).sort({ date: 1 });
}

export async function createHoliday(name: string, date: number, isRecurring?: boolean) {
  return db.holidays.create({ name, date, ...(isRecurring !== undefined && { isRecurring }) });
}

export async function getLeaveReport() {
  return db.leaveRequests.aggregate([
    { $match: { status: "approved" } },
    {
      $group: {
        _id: "$type",
        totalRequests: { $sum: 1 },
        totalDays:     { $sum: "$days" },
        avgDays:       { $avg: "$days" },
      },
    },
    { $sort: { totalDays: -1 } },
  ]);
}

// ---------------------------------------------------------------------------
// Payroll (8 endpoints)
// ---------------------------------------------------------------------------

export async function createPayrollRun(period: string, processedBy: number) {
  return db.payrollRuns.create({
    period,
    runDate: Date.now(),
    processedBy,
  });
}

export async function processPayroll(payrollRunId: number) {
  await db.payrollRuns.updateOne({ id: payrollRunId }, { status: "processing" });

  const { data: activeEmployees } = await db.employees.find({ status: "active" });
  if (!activeEmployees || (activeEmployees as Record<string, unknown>[]).length === 0) {
    return { data: { processed: 0 }, error: null };
  }

  let totalGross = 0;
  let totalNet   = 0;
  let totalDeductions = 0;

  for (const emp of activeEmployees as Record<string, unknown>[]) {
    const base = (emp.salary as number) || 0;
    const deductionsTax      = Math.round(base * 0.22);
    const deductionsBenefits = Math.round(base * 0.05);
    const deductionsOther    = 0;
    const net = base - deductionsTax - deductionsBenefits - deductionsOther;

    await db.payslips.create({
      payrollRunId,
      employeeId: emp._id as number,
      baseSalary: base,
      deductionsTax,
      deductionsBenefits,
      deductionsOther,
      netPay: net,
    });

    totalGross += base;
    totalNet   += net;
    totalDeductions += deductionsTax + deductionsBenefits;
  }

  await db.payrollRuns.updateOne(
    { id: payrollRunId },
    {
      status: "completed",
      totalGross,
      totalNet,
      totalDeductions,
    }
  );

  return {
    data: {
      processed: (activeEmployees as Record<string, unknown>[]).length,
      totalGross,
      totalNet,
    },
    error: null,
  };
}

export async function finalizePayroll(payrollRunId: number) {
  await db.payslips.updateMany({ payrollRunId, status: "pending" }, { status: "paid" });
  return { data: { finalized: true }, error: null };
}

export async function getPayrollRuns(filters?: Record<string, unknown>) {
  return db.payrollRuns.find(filters || {}).sort({ runDate: -1 });
}

export async function getPayslip(id: number) {
  return db.payslips.findOne({ id });
}

export async function getPayslipsByEmployee(employeeId: number) {
  return db.payslips.find({ employeeId }).sort({ createdAt: -1 });
}

export async function getPayrollSummary(period: string) {
  const { data: run } = await db.payrollRuns.findOne({ period });
  if (!run) return { data: null, error: { message: "Payroll run not found" } };

  const { data: slips } = await db.payslips.find({
    payrollRunId: (run as Record<string, unknown>)._id as number,
  });

  return {
    data: {
      run,
      employeeCount: ((slips as Record<string, unknown>[]) || []).length,
      slips,
    },
    error: null,
  };
}

export async function exportPayroll(payrollRunId: number) {
  const { data: slips } = await db.payslips.find({ payrollRunId });
  if (!slips) return { data: "", error: null };

  const rows = slips as Record<string, unknown>[];
  const header = "employeeId,baseSalary,overtimePay,bonus,deductionsTax,deductionsBenefits,deductionsOther,netPay,status";
  const lines = rows.map(r =>
    [
      r.employeeId, r.baseSalary, r.overtimePay, r.bonus,
      r.deductionsTax, r.deductionsBenefits, r.deductionsOther,
      r.netPay, r.status,
    ].join(",")
  );
  return { data: [header, ...lines].join("\n"), error: null };
}

// ---------------------------------------------------------------------------
// Performance (10 endpoints)
// ---------------------------------------------------------------------------

export async function createReview(
  employeeId: number,
  reviewerId: number,
  period: string,
  cycle: string,
  strengths?: string,
  improvements?: string,
  goalsText?: string
) {
  return db.reviews.create({
    employeeId, reviewerId, period, cycle,
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

export async function getReviewsForEmployee(employeeId: number) {
  return db.reviews.find({ employeeId }).sort({ createdAt: -1 });
}

export async function getPendingReviews(reviewerId?: number) {
  return db.reviews.find({
    status: "draft",
    ...(reviewerId !== undefined && { reviewerId }),
  }).sort({ createdAt: 1 });
}

export async function createGoal(
  employeeId: number,
  title: string,
  category: string,
  targetDate?: number,
  description?: string
) {
  return db.goals.create({
    employeeId, title, category,
    ...(targetDate  !== undefined && { targetDate }),
    ...(description && { description }),
  });
}

export async function updateGoalProgress(id: number, progress: number, status?: string) {
  return db.goals.updateOne({ id }, {
    progress,
    ...(status && { status }),
  });
}

export async function getGoals(employeeId: number, filters?: Record<string, unknown>) {
  return db.goals.find({ employeeId, ...(filters || {}) }).sort({ targetDate: 1 });
}

export async function giveFeedback(
  fromEmployeeId: number,
  toEmployeeId: number,
  type: string,
  message: string,
  isAnonymous?: boolean
) {
  return db.feedback.create({
    fromEmployeeId, toEmployeeId, type, message,
    ...(isAnonymous !== undefined && { isAnonymous }),
  });
}

export async function getFeedbackForEmployee(toEmployeeId: number) {
  return db.feedback.find({ toEmployeeId }).sort({ createdAt: -1 });
}

// ---------------------------------------------------------------------------
// Training (8 endpoints)
// ---------------------------------------------------------------------------

export async function createCourse(
  title: string,
  category: string,
  durationHours: number,
  description?: string,
  isMandatory?: boolean,
  maxParticipants?: number
) {
  return db.courses.create({
    title, category, durationHours,
    ...(description    && { description }),
    ...(isMandatory    !== undefined && { isMandatory }),
    ...(maxParticipants !== undefined && { maxParticipants }),
  });
}

export async function getCourses(filters?: Record<string, unknown>) {
  return db.courses.find(filters || {}).sort({ title: 1 });
}

export async function enrollInCourse(courseId: number, employeeId: number) {
  return db.enrollments.create({
    courseId, employeeId,
    enrolledAt: Date.now(),
  });
}

export async function completeCourse(
  courseId: number,
  employeeId: number,
  score?: number
) {
  return db.enrollments.updateOne(
    { courseId, employeeId },
    {
      status: "completed",
      completedAt: Date.now(),
      ...(score !== undefined && { score }),
    }
  );
}

export async function getEnrollments(filters?: Record<string, unknown>) {
  return db.enrollments.find(filters || {}).sort({ enrolledAt: -1 });
}

export async function addCertification(
  employeeId: number,
  name: string,
  issuer: string,
  issueDate: number,
  expiryDate?: number,
  credentialUrl?: string
) {
  return db.certifications.create({
    employeeId, name, issuer, issueDate,
    ...(expiryDate    !== undefined && { expiryDate }),
    ...(credentialUrl && { credentialUrl }),
  });
}

export async function getCertifications(employeeId: number) {
  return db.certifications.find({ employeeId }).sort({ issueDate: -1 });
}

export async function getExpiringCertifications(daysAhead: number = 30) {
  const now    = Date.now();
  const cutoff = now + daysAhead * 86_400_000;
  return db.certifications.find({
    expiryDate: { $gte: now, $lte: cutoff },
  }).sort({ expiryDate: 1 });
}

// ---------------------------------------------------------------------------
// Compensation & Benefits (8 endpoints)
// ---------------------------------------------------------------------------

export async function adjustSalary(
  employeeId: number,
  newSalary: number,
  changeType: string,
  effectiveDate: number,
  changeReason?: string,
  approvedBy?: number
) {
  await db.employees.updateOne({ id: employeeId }, { salary: newSalary });

  return db.compensationHistory.create({
    employeeId,
    effectiveDate,
    salary: newSalary,
    changeType,
    ...(changeReason && { changeReason }),
    ...(approvedBy !== undefined && { approvedBy }),
  });
}

export async function getCompensationHistory(employeeId: number) {
  return db.compensationHistory.find({ employeeId }).sort({ effectiveDate: -1 });
}

export async function getBenefitsPlans(type?: string) {
  return db.benefitsPlans.find(type ? { type } : {}).sort({ name: 1 });
}

export async function enrollInBenefit(
  employeeId: number,
  planId: number,
  startDate: number
) {
  return db.benefitsEnrollments.create({ employeeId, planId, startDate });
}

export async function getBenefitsEnrollments(employeeId: number) {
  return db.benefitsEnrollments.find({ employeeId, status: "active" }).sort({ startDate: -1 });
}

export async function submitExpense(
  employeeId: number,
  description: string,
  amount: number,
  category: string,
  receiptUrl?: string
) {
  return db.expenseClaims.create({
    employeeId, description, amount, category,
    submittedAt: Date.now(),
    ...(receiptUrl && { receiptUrl }),
  });
}

export async function approveExpense(id: number, approvedBy: number, approve: boolean) {
  return db.expenseClaims.updateOne(
    { id },
    { status: approve ? "approved" : "rejected", approvedBy }
  );
}

export async function getExpenses(filters?: Record<string, unknown>) {
  return db.expenseClaims.find(filters || {}).sort({ submittedAt: -1 });
}

// ---------------------------------------------------------------------------
// Documents & Compliance (6 endpoints)
// ---------------------------------------------------------------------------

export async function uploadDocument(
  employeeId: number,
  type: string,
  name: string,
  fileUrl: string,
  expiresAt?: number
) {
  return db.documents.create({
    employeeId, type, name, fileUrl,
    uploadedAt: Date.now(),
    ...(expiresAt !== undefined && { expiresAt }),
  });
}

export async function getDocuments(employeeId: number, type?: string) {
  return db.documents.find({
    employeeId,
    ...(type && { type }),
  }).sort({ uploadedAt: -1 });
}

export async function getExpiringDocuments(daysAhead: number = 30) {
  const now    = Date.now();
  const cutoff = now + daysAhead * 86_400_000;
  return db.documents.find({
    expiresAt: { $gte: now, $lte: cutoff },
  }).sort({ expiresAt: 1 });
}

export async function getPolicies(category?: string) {
  return db.policies.find(category ? { category } : {}).sort({ effectiveDate: -1 });
}

export async function getAuditLog(
  entityType?: string,
  entityId?: number,
  actorId?: number
) {
  return db.auditLog.find({
    ...(entityType !== undefined && { entityType }),
    ...(entityId   !== undefined && { entityId }),
    ...(actorId    !== undefined && { actorId }),
  }).sort({ timestamp: -1 }).limit(500);
}

export async function acknowledgePolicy(employeeId: number, policyId: number) {
  // Record acknowledgement as a document
  return db.documents.create({
    employeeId,
    type: "policy",
    name: `Policy ${policyId} Acknowledgement`,
    fileUrl: "",
    uploadedAt: Date.now(),
  });
}

// ---------------------------------------------------------------------------
// Notifications (4 endpoints)
// ---------------------------------------------------------------------------

export async function getNotifications(employeeId: number) {
  return db.notifications.find({ employeeId }).sort({ createdAt: -1 });
}

export async function markAsRead(id: number) {
  return db.notifications.updateOne({ id }, { isRead: true });
}

export async function markAllAsRead(employeeId: number) {
  return db.notifications.updateMany({ employeeId, isRead: false }, { isRead: true });
}

export async function getUnreadCount(employeeId: number) {
  return db.notifications.countDocuments({ employeeId, isRead: false });
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
    db.positions.countDocuments({ isOpen: true }),
    db.leaveRequests.countDocuments({ status: "pending" }),
    db.jobPostings.countDocuments({ status: "open" }),
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
    { $group: { _id: "$departmentId", headcount: { $sum: 1 }, avgSalary: { $avg: "$salary" } } },
    { $sort: { headcount: -1 } },
  ]);
}

export async function getSalaryDistribution() {
  return db.employees.aggregate([
    { $match: { status: "active" } },
    {
      $group: {
        _id: "$departmentId",
        minSalary:    { $min: "$salary" },
        maxSalary:    { $max: "$salary" },
        avgSalary:    { $avg: "$salary" },
        totalPayroll: { $sum: "$salary" },
        headcount:    { $sum: 1 },
      },
    },
    { $sort: { totalPayroll: -1 } },
  ]);
}

export async function getAttritionReport() {
  const { data: terminated } = await db.employees.countDocuments({ status: "terminated" });
  const { data: total }      = await db.employees.countDocuments({});
  const rate = total ? (((terminated as number) || 0) / (total as number) * 100).toFixed(1) : "0.0";
  return { data: { terminated, total, attritionRate: `${rate}%` }, error: null };
}

export async function getLeaveUtilization() {
  return db.leaveRequests.aggregate([
    { $match: { status: "approved" } },
    {
      $group: {
        _id: "$employeeId",
        totalDaysUsed: { $sum: "$days" },
        requestsCount: { $sum: 1 },
      },
    },
    { $sort: { totalDaysUsed: -1 } },
  ]);
}

export async function getReviewStats() {
  return db.reviews.aggregate([
    { $group: { _id: "$period", count: { $sum: 1 }, avgRating: { $avg: "$rating" } } },
    { $sort: { _id: -1 } },
  ]);
}

export async function getTimeToHire() {
  // Hired applicants carry the appliedDate; the job posting has the postedDate.
  // We aggregate by jobPostingId and compute avg days from application to hire.
  return db.applicants.aggregate([
    { $match: { stage: "hired" } },
    {
      $group: {
        _id: "$jobPostingId",
        count: { $sum: 1 },
        avgApplyDate: { $avg: "$appliedDate" },
      },
    },
    { $sort: { count: -1 } },
  ]);
}

export async function getDiversityMetrics() {
  // Department composition with skill breakdown
  return db.employees.aggregate([
    { $match: { status: "active" } },
    { $group: { _id: "$departmentId", headcount: { $sum: 1 } } },
    { $sort: { headcount: -1 } },
  ]);
}

export async function getCostPerEmployee() {
  return db.employees.aggregate([
    { $match: { status: "active" } },
    {
      $group: {
        _id: "$departmentId",
        totalSalary: { $sum: "$salary" },
        headcount:   { $sum: 1 },
        avgSalary:   { $avg: "$salary" },
      },
    },
    { $sort: { totalSalary: -1 } },
  ]);
}

export async function getMonthlyTrends() {
  const { data: hires } = await db.employees.aggregate([
    { $group: { _id: { $dateToString: { format: "%Y-%m", date: { $toDate: "$hireDate" } } }, count: { $sum: 1 } } },
    { $sort: { _id: 1 } },
  ]);

  const { data: terminations } = await db.employees.aggregate([
    { $match: { status: "terminated" } },
    { $group: { _id: { $dateToString: { format: "%Y-%m", date: { $toDate: "$hireDate" } } }, count: { $sum: 1 } } },
    { $sort: { _id: 1 } },
  ]);

  return { data: { hires, terminations }, error: null };
}
