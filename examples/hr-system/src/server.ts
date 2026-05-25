"use server";

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

import { schema, t } from "@zeroship/db";
import { env } from "zeroship";
import { procedure } from "@zeroship/rpc/server";

// Schema declared via the `export default { schema }` convention; the
// platform installs typed Collection wrappers on `env.db` at app boot.
const dbSchema = {
  // ---------------------------------------------------------------------------
  // Core
  // ---------------------------------------------------------------------------
  departments: {
    name:               t.string().required(),
    code:               t.string().required(),
    managerId:          t.number(),
    budget:             t.number().default(0),
    headcount:          t.number().default(0),
    parentDepartmentId: t.number(),
  },

  positions: {
    title:        t.string().required(),
    departmentId: t.number().required(),
    level:        t.string().enum("junior", "mid", "senior", "lead", "director", "vp", "c-level"),
    salaryMin:    t.number(),
    salaryMax:    t.number(),
    isOpen:       t.boolean().default(true),
    description:  t.string(),
  },

  employees: schema({
    firstName:             t.string().required(),
    lastName:              t.string().required(),
    email:                 t.string().required(),
    phone:                 t.string(),
    departmentId:          t.number(),
    positionId:            t.number(),
    managerId:             t.number(),
    hireDate:              t.number(),
    salary:                t.number(),
    status:                t.string().enum("active", "on_leave", "terminated").default("active"),
    skills:                t.array(t.string()),
    avatarUrl:             t.string(),
    emergencyContactName:  t.string(),
    emergencyContactPhone: t.string(),
    address:               t.string(),
    dateOfBirth:           t.number(),
  }).softDelete(),

  // ---------------------------------------------------------------------------
  // Recruitment
  // ---------------------------------------------------------------------------
  jobPostings: {
    positionId:  t.number().required(),
    title:       t.string().required(),
    description: t.string(),
    requirements:t.string(),
    status:      t.string().enum("draft", "open", "closed").default("draft"),
    postedDate:  t.number(),
    closingDate: t.number(),
  },

  applicants: {
    jobPostingId: t.number().required(),
    name:         t.string().required(),
    email:        t.string().required(),
    phone:        t.string(),
    resumeUrl:    t.string(),
    stage:        t.string().enum("applied", "screening", "interview", "offer", "hired", "rejected").default("applied"),
    rating:       t.number(),
    notes:        t.string(),
    appliedDate:  t.number(),
  },

  interviews: {
    applicantId:     t.number().required(),
    interviewerId:   t.number().required(),
    scheduledAt:     t.number().required(),
    durationMinutes: t.number().default(60),
    type:            t.string().enum("phone", "video", "onsite").default("video"),
    status:          t.string().enum("scheduled", "completed", "cancelled").default("scheduled"),
    feedback:        t.string(),
    rating:          t.number(),
  },

  // ---------------------------------------------------------------------------
  // Time & Attendance
  // ---------------------------------------------------------------------------
  timesheets: {
    employeeId:    t.number().required(),
    date:          t.number().required(),
    clockIn:       t.number(),
    clockOut:      t.number(),
    hoursWorked:   t.number().default(0),
    overtimeHours: t.number().default(0),
    status:        t.string().enum("draft", "submitted", "approved").default("draft"),
    notes:         t.string(),
  },

  workSchedules: {
    employeeId: t.number().required(),
    dayOfWeek:  t.number().required(), // 0=Sun, 6=Sat
    startTime:  t.string().required(),
    endTime:    t.string().required(),
    isRemote:   t.boolean().default(false),
  },

  // ---------------------------------------------------------------------------
  // Leave
  // ---------------------------------------------------------------------------
  leaveRequests: {
    employeeId: t.number().required(),
    type:       t.string().required().enum("vacation", "sick", "personal", "parental", "bereavement"),
    startDate:  t.number().required(),
    endDate:    t.number().required(),
    days:       t.number().required(),
    reason:     t.string(),
    status:     t.string().enum("pending", "approved", "denied", "cancelled").default("pending"),
    approvedBy: t.number(),
    approvedAt: t.number(),
  },

  leaveBalances: {
    employeeId:    t.number().required(),
    year:          t.number().required(),
    vacationTotal: t.number().default(20),
    vacationUsed:  t.number().default(0),
    sickTotal:     t.number().default(10),
    sickUsed:      t.number().default(0),
    personalTotal: t.number().default(5),
    personalUsed:  t.number().default(0),
  },

  holidays: {
    name:        t.string().required(),
    date:        t.number().required(),
    isRecurring: t.boolean().default(false),
  },

  // ---------------------------------------------------------------------------
  // Payroll
  // ---------------------------------------------------------------------------
  payrollRuns: {
    period:          t.string().required(),
    runDate:         t.number(),
    status:          t.string().enum("draft", "processing", "completed").default("draft"),
    totalGross:      t.number().default(0),
    totalNet:        t.number().default(0),
    totalDeductions: t.number().default(0),
    processedBy:     t.number(),
  },

  payslips: {
    payrollRunId:       t.number().required(),
    employeeId:         t.number().required(),
    baseSalary:         t.number().required(),
    overtimePay:        t.number().default(0),
    bonus:              t.number().default(0),
    deductionsTax:      t.number().default(0),
    deductionsBenefits: t.number().default(0),
    deductionsOther:    t.number().default(0),
    netPay:             t.number().required(),
    status:             t.string().enum("pending", "paid").default("pending"),
  },

  // ---------------------------------------------------------------------------
  // Performance
  // ---------------------------------------------------------------------------
  reviews: {
    employeeId:  t.number().required(),
    reviewerId:  t.number().required(),
    period:      t.string().required(),
    cycle:       t.string().enum("quarterly", "annual").default("annual"),
    rating:      t.number().min(1).max(5),
    strengths:   t.string(),
    improvements:t.string(),
    goals:       t.string(),
    status:      t.string().enum("draft", "submitted", "acknowledged").default("draft"),
  },

  goals: {
    employeeId:  t.number().required(),
    title:       t.string().required(),
    description: t.string(),
    targetDate:  t.number(),
    status:      t.string().enum("active", "completed", "cancelled").default("active"),
    progress:    t.number().default(0).min(0).max(100),
    category:    t.string().enum("performance", "development", "project").default("performance"),
  },

  feedback: {
    fromEmployeeId: t.number().required(),
    toEmployeeId:   t.number().required(),
    type:           t.string().enum("praise", "constructive").required(),
    message:        t.string().required(),
    isAnonymous:    t.boolean().default(false),
  },

  // ---------------------------------------------------------------------------
  // Training
  // ---------------------------------------------------------------------------
  courses: {
    title:           t.string().required(),
    description:     t.string(),
    category:        t.string(),
    durationHours:   t.number(),
    isMandatory:     t.boolean().default(false),
    maxParticipants: t.number(),
  },

  enrollments: {
    courseId:     t.number().required(),
    employeeId:  t.number().required(),
    status:      t.string().enum("enrolled", "in_progress", "completed", "dropped").default("enrolled"),
    enrolledAt:  t.number(),
    completedAt: t.number(),
    score:       t.number(),
  },

  certifications: {
    employeeId:    t.number().required(),
    name:          t.string().required(),
    issuer:        t.string(),
    issueDate:     t.number(),
    expiryDate:    t.number(),
    credentialUrl: t.string(),
  },

  // ---------------------------------------------------------------------------
  // Compensation & Benefits
  // ---------------------------------------------------------------------------
  compensationHistory: {
    employeeId:    t.number().required(),
    effectiveDate: t.number().required(),
    salary:        t.number().required(),
    changeType:    t.string().enum("hire", "promotion", "adjustment", "annual").required(),
    changeReason:  t.string(),
    approvedBy:    t.number(),
  },

  benefitsPlans: {
    name:                  t.string().required(),
    type:                  t.string().enum("health", "dental", "vision", "life", "retirement").required(),
    provider:              t.string(),
    monthlyCostEmployee:   t.number().default(0),
    monthlyCostEmployer:   t.number().default(0),
  },

  benefitsEnrollments: {
    employeeId: t.number().required(),
    planId:     t.number().required(),
    startDate:  t.number().required(),
    endDate:    t.number(),
    status:     t.string().enum("active", "cancelled").default("active"),
  },

  expenseClaims: {
    employeeId:  t.number().required(),
    description: t.string().required(),
    amount:      t.number().required(),
    category:    t.string().enum("travel", "meals", "equipment", "other").required(),
    receiptUrl:  t.string(),
    status:      t.string().enum("submitted", "approved", "rejected", "reimbursed").default("submitted"),
    submittedAt: t.number(),
    approvedBy:  t.number(),
  },

  // ---------------------------------------------------------------------------
  // Documents & Compliance
  // ---------------------------------------------------------------------------
  documents: {
    employeeId: t.number().required(),
    type:       t.string().enum("contract", "id", "certification", "policy", "other").required(),
    name:       t.string().required(),
    fileUrl:    t.string().required(),
    uploadedAt: t.number(),
    expiresAt:  t.number(),
  },

  auditLog: {
    actorId:    t.number().required(),
    action:     t.string().enum("create", "update", "delete").required(),
    entityType: t.string().required(),
    entityId:   t.number().required(),
    changesJson:t.string(),
    timestamp:  t.number(),
  },

  policies: {
    title:         t.string().required(),
    content:       t.string(),
    version:       t.string(),
    effectiveDate: t.number(),
    category:      t.string().enum("handbook", "conduct", "safety", "privacy").required(),
  },

  // ---------------------------------------------------------------------------
  // Notifications
  // ---------------------------------------------------------------------------
  notifications: {
    employeeId: t.number().required(),
    type:       t.string().enum("leave_approved", "review_due", "payroll_ready", "course_reminder", "general").required(),
    title:      t.string().required(),
    message:    t.string(),
    isRead:     t.boolean().default(false),
    link:       t.string(),
  },
};

export default { schema: dbSchema };

const db = env.db;

// ===========================================================================
// API Endpoints
// ===========================================================================

// ---------------------------------------------------------------------------
// Employee Management (12 endpoints)
// ---------------------------------------------------------------------------

export const createEmployee = procedure(async (
  firstName: string,
  lastName: string,
  email: string,
  departmentId: number,
  positionId: number,
  salary: number,
  extras?: Record<string, unknown>
) => {
  const { data, error } = await db.employees.insert({
    firstName, lastName, email,
    departmentId, positionId, salary,
    hireDate: Date.now(),
    status: "active",
    ...(extras || {}),
  });
  if (error) return { data: null, error };

  await db.departments.update({ id: departmentId }, { headcount: { $inc: 1 } });

  // Record initial compensation history
  await db.compensationHistory.insert({
    employeeId: (data as Record<string, unknown>)._id as number,
    effectiveDate: Date.now(),
    salary,
    changeType: "hire",
  });

  return { data, error: null };
}, { id: "hr.createEmployee" });

export const getEmployees = procedure(async (filters?: Record<string, unknown>) => {
  return db.employees.find(filters || {}).sort({ lastName: 1, firstName: 1 });
}, { id: "hr.getEmployees" });

export const getEmployee = procedure(async (id: number) => {
  return db.employees.get({ id });
}, { id: "hr.getEmployee" });

export const updateEmployee = procedure(async (id: number, changes: Record<string, unknown>) => {
  return db.employees.update({ id }, changes);
}, { id: "hr.updateEmployee" });

export const terminateEmployee = procedure(async (id: number) => {
  const { data: emp } = await db.employees.get({ id });
  if (!emp) return { data: null, error: { message: "Employee not found" } };

  await db.employees.update({ id }, { status: "terminated" });

  if ((emp as Record<string, unknown>).departmentId) {
    await db.departments.update(
      { id: (emp as Record<string, unknown>).departmentId as number },
      { headcount: { $inc: -1 } }
    );
  }

  return { data: { terminated: true }, error: null };
}, { id: "hr.terminateEmployee" });

export const getOrgChart = procedure(async () => {
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
}, { id: "hr.getOrgChart" });

export const getDirectReports = procedure(async (managerId: number) => {
  return db.employees.find({ managerId, status: "active" }).sort({ lastName: 1 });
}, { id: "hr.getDirectReports" });

export const searchEmployees = procedure(async (query: string) => {
  return db.employees.find({
    $or: [
      { firstName: { $ilike: `%${query}%` } },
      { lastName:  { $ilike: `%${query}%` } },
      { email:     { $ilike: `%${query}%` } },
    ],
  }).limit(20);
}, { id: "hr.searchEmployees" });

export const addSkill = procedure(async (id: number, skill: string) => {
  return db.employees.update({ id }, { skills: { $addToSet: skill } });
}, { id: "hr.addSkill" });

export const removeSkill = procedure(async (id: number, skill: string) => {
  return db.employees.update({ id }, { skills: { $pull: skill } });
}, { id: "hr.removeSkill" });

export const getEmployeesByDepartment = procedure(async (departmentId: number) => {
  return db.employees.find({ departmentId, status: "active" }).sort({ lastName: 1 });
}, { id: "hr.getEmployeesByDepartment" });

export const getEmployeeHistory = procedure(async (employeeId: number) => {
  const [compHistory, posHistory] = await Promise.all([
    db.compensationHistory.find({ employeeId }).sort({ effectiveDate: -1 }),
    db.reviews.find({ employeeId }).sort({ created_at: -1 }),
  ]);
  return {
    data: {
      compensation: (compHistory as Record<string, unknown>).data,
      reviews: (posHistory as Record<string, unknown>).data,
    },
    error: null,
  };
}, { id: "hr.getEmployeeHistory" });

// ---------------------------------------------------------------------------
// Department (6 endpoints)
// ---------------------------------------------------------------------------

export const createDepartment = procedure(async (
  name: string,
  code: string,
  budget?: number,
  parentDepartmentId?: number
) => {
  return db.departments.insert({
    name,
    code,
    ...(budget !== undefined && { budget }),
    ...(parentDepartmentId !== undefined && { parentDepartmentId }),
  });
}, { id: "hr.createDepartment" });

export const getDepartments = procedure(async () => {
  return db.departments.find({}).sort({ name: 1 });
}, { id: "hr.getDepartments" });

export const getDepartment = procedure(async (id: number) => {
  return db.departments.get({ id });
}, { id: "hr.getDepartment" });

export const updateDepartment = procedure(async (id: number, changes: Record<string, unknown>) => {
  return db.departments.update({ id }, changes);
}, { id: "hr.updateDepartment" });

export const deleteDepartment = procedure(async (id: number) => {
  return db.departments.delete({ id });
}, { id: "hr.deleteDepartment" });

export const getDepartmentTree = procedure(async () => {
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
}, { id: "hr.getDepartmentTree" });

// ---------------------------------------------------------------------------
// Position (6 endpoints)
// ---------------------------------------------------------------------------

export const createPosition = procedure(async (
  title: string,
  departmentId: number,
  level: string,
  salaryMin: number,
  salaryMax: number,
  description?: string
) => {
  return db.positions.insert({ title, departmentId, level, salaryMin, salaryMax, ...(description && { description }) });
}, { id: "hr.createPosition" });

export const getPositions = procedure(async (filters?: Record<string, unknown>) => {
  return db.positions.find(filters || {}).sort({ title: 1 });
}, { id: "hr.getPositions" });

export const getOpenPositions = procedure(async () => {
  return db.positions.find({ isOpen: true }).sort({ title: 1 });
}, { id: "hr.getOpenPositions" });

export const updatePosition = procedure(async (id: number, changes: Record<string, unknown>) => {
  return db.positions.update({ id }, changes);
}, { id: "hr.updatePosition" });

export const closePosition = procedure(async (id: number) => {
  return db.positions.update({ id }, { isOpen: false });
}, { id: "hr.closePosition" });

export const getPositionsByDepartment = procedure(async (departmentId: number) => {
  return db.positions.find({ departmentId }).sort({ level: 1 });
}, { id: "hr.getPositionsByDepartment" });

// ---------------------------------------------------------------------------
// Recruitment (10 endpoints)
// ---------------------------------------------------------------------------

export const createJobPosting = procedure(async (
  positionId: number,
  title: string,
  description?: string,
  requirements?: string,
  closingDate?: number
) => {
  return db.jobPostings.insert({
    positionId, title,
    ...(description && { description }),
    ...(requirements && { requirements }),
    ...(closingDate && { closingDate }),
  });
}, { id: "hr.createJobPosting" });

export const getJobPostings = procedure(async (filters?: Record<string, unknown>) => {
  return db.jobPostings.find(filters || {}).sort({ created_at: -1 });
}, { id: "hr.getJobPostings" });

export const getJobPosting = procedure(async (id: number) => {
  return db.jobPostings.get({ id });
}, { id: "hr.getJobPosting" });

export const publishJobPosting = procedure(async (id: number) => {
  return db.jobPostings.update({ id }, { status: "open", postedDate: Date.now() });
}, { id: "hr.publishJobPosting" });

export const closeJobPosting = procedure(async (id: number) => {
  return db.jobPostings.update({ id }, { status: "closed" });
}, { id: "hr.closeJobPosting" });

export const applyToJob = procedure(async (
  jobPostingId: number,
  name: string,
  email: string,
  phone?: string,
  resumeUrl?: string
) => {
  return db.applicants.insert({
    jobPostingId, name, email,
    ...(phone && { phone }),
    ...(resumeUrl && { resumeUrl }),
    appliedDate: Date.now(),
  });
}, { id: "hr.applyToJob" });

export const getApplicants = procedure(async (jobPostingId: number, stage?: string) => {
  return db.applicants.find({
    jobPostingId,
    ...(stage && { stage }),
  }).sort({ appliedDate: -1 });
}, { id: "hr.getApplicants" });

export const updateApplicantStage = procedure(async (
  id: number,
  stage: string,
  notes?: string,
  rating?: number
) => {
  return db.applicants.update({ id }, {
    stage,
    ...(notes !== undefined && { notes }),
    ...(rating !== undefined && { rating }),
  });
}, { id: "hr.updateApplicantStage" });

export const scheduleInterview = procedure(async (
  applicantId: number,
  interviewerId: number,
  scheduledAt: number,
  type: string,
  durationMinutes?: number
) => {
  return db.interviews.insert({
    applicantId, interviewerId, scheduledAt, type,
    ...(durationMinutes && { durationMinutes }),
  });
}, { id: "hr.scheduleInterview" });

export const submitInterviewFeedback = procedure(async (
  id: number,
  feedback: string,
  rating: number
) => {
  return db.interviews.update({ id }, { status: "completed", feedback, rating });
}, { id: "hr.submitInterviewFeedback" });

// ---------------------------------------------------------------------------
// Time & Attendance (8 endpoints)
// ---------------------------------------------------------------------------

export const clockIn = procedure(async (employeeId: number, date: number, notes?: string) => {
  // Check if a timesheet already exists for today
  const { data: existing } = await db.timesheets.get({ employeeId, date });
  if (existing) {
    return { data: null, error: { message: "Already clocked in for this date" } };
  }
  return db.timesheets.insert({
    employeeId,
    date,
    clockIn: Date.now(),
    ...(notes && { notes }),
  });
}, { id: "hr.clockIn" });

export const clockOut = procedure(async (employeeId: number, date: number) => {
  const { data: ts } = await db.timesheets.get({ employeeId, date });
  if (!ts) return { data: null, error: { message: "No clock-in found for this date" } };

  const tsData = ts as Record<string, unknown>;
  const clockOutTime = Date.now();
  const clockInTime = tsData.clockIn as number;
  const ms = clockOutTime - clockInTime;
  const hoursWorked = Math.round((ms / 3_600_000) * 100) / 100;
  const overtimeHours = Math.max(0, Math.round((hoursWorked - 8) * 100) / 100);

  return db.timesheets.update(
    { id: tsData._id as number },
    { clockOut: clockOutTime, hoursWorked, overtimeHours }
  );
}, { id: "hr.clockOut" });

export const submitTimesheet = procedure(async (id: number) => {
  return db.timesheets.update({ id }, { status: "submitted" });
}, { id: "hr.submitTimesheet" });

export const approveTimesheet = procedure(async (id: number) => {
  return db.timesheets.update({ id }, { status: "approved" });
}, { id: "hr.approveTimesheet" });

export const getTimesheets = procedure(async (
  employeeId: number,
  fromDate?: number,
  toDate?: number
) => {
  return db.timesheets.find({
    employeeId,
    ...(fromDate !== undefined && toDate !== undefined && {
      date: { $gte: fromDate, $lte: toDate },
    }),
  }).sort({ date: -1 });
}, { id: "hr.getTimesheets" });

export const getWorkSchedule = procedure(async (employeeId: number) => {
  return db.workSchedules.find({ employeeId }).sort({ dayOfWeek: 1 });
}, { id: "hr.getWorkSchedule" });

export const updateWorkSchedule = procedure(async (
  employeeId: number,
  dayOfWeek: number,
  startTime: string,
  endTime: string,
  isRemote?: boolean
) => {
  const { data: existing } = await db.workSchedules.get({ employeeId, dayOfWeek });
  if (existing) {
    return db.workSchedules.update(
      { id: (existing as Record<string, unknown>)._id as number },
      { startTime, endTime, ...(isRemote !== undefined && { isRemote }) }
    );
  }
  return db.workSchedules.insert({
    employeeId, dayOfWeek, startTime, endTime,
    ...(isRemote !== undefined && { isRemote }),
  });
}, { id: "hr.updateWorkSchedule" });

export const getOvertimeReport = procedure(async (fromDate?: number, toDate?: number) => {
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
}, { id: "hr.getOvertimeReport" });

// ---------------------------------------------------------------------------
// Leave (10 endpoints)
// ---------------------------------------------------------------------------

export const requestLeave = procedure(async (
  employeeId: number,
  type: string,
  startDate: number,
  endDate: number,
  days: number,
  reason?: string
) => {
  return db.leaveRequests.insert({
    employeeId, type, startDate, endDate, days,
    ...(reason && { reason }),
  });
}, { id: "hr.requestLeave" });

export const approveLeave = procedure(async (id: number, approvedBy: number) => {
  // `update(filter, patch)` returns the updated row (or null when nothing
  // matched). The pre-clean-rename code used `updateOne` and inspected
  // `{matchedCount, modifiedCount}`; the row-return shape is more direct.
  const { data: leave } = await db.leaveRequests.update(
    { id, status: "pending" },
    { status: "approved", approvedBy, approvedAt: Date.now() }
  );

  if (leave) {
    const leaveData = leave as Record<string, unknown>;
    await db.employees.update(
      { id: leaveData.employeeId as number },
      { status: "on_leave" }
    );
  }

  return { data: leave, error: null };
}, { id: "hr.approveLeave" });

export const denyLeave = procedure(async (id: number, approvedBy: number) => {
  return db.leaveRequests.update(
    { id, status: "pending" },
    { status: "denied", approvedBy, approvedAt: Date.now() }
  );
}, { id: "hr.denyLeave" });

export const cancelLeave = procedure(async (id: number, employeeId: number) => {
  return db.leaveRequests.update(
    { id, employeeId, status: "pending" },
    { status: "cancelled" }
  );
}, { id: "hr.cancelLeave" });

export const getLeaveRequests = procedure(async (filters?: Record<string, unknown>) => {
  return db.leaveRequests.find(filters || {}).sort({ created_at: -1 });
}, { id: "hr.getLeaveRequests" });

export const getLeaveBalance = procedure(async (employeeId: number) => {
  const currentYear = new Date().getFullYear();

  // Check if there's an explicit balance record
  const { data: balanceRecord } = await db.leaveBalances.get({ employeeId, year: currentYear });

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
}, { id: "hr.getLeaveBalance" });

export const getLeaveCalendar = procedure(async (departmentId?: number) => {
  const empFilter: Record<string, unknown> = { status: { $ne: "terminated" } };
  if (departmentId !== undefined) empFilter.departmentId = departmentId;

  const { data: teamMembers } = await db.employees.find(empFilter);
  const ids = ((teamMembers as Record<string, unknown>[]) || []).map(e => (e as Record<string, unknown>)._id as number);

  if (ids.length === 0) return { data: [], error: null };

  return db.leaveRequests.find({
    employeeId: { $in: ids },
    status: "approved",
  }).sort({ startDate: 1 });
}, { id: "hr.getLeaveCalendar" });

export const getHolidays = procedure(async (year?: number) => {
  if (!year) return db.holidays.find({}).sort({ date: 1 });
  const start = new Date(year, 0, 1).getTime();
  const end   = new Date(year, 11, 31, 23, 59, 59).getTime();
  return db.holidays.find({ date: { $gte: start, $lte: end } }).sort({ date: 1 });
}, { id: "hr.getHolidays" });

export const createHoliday = procedure(async (name: string, date: number, isRecurring?: boolean) => {
  return db.holidays.insert({ name, date, ...(isRecurring !== undefined && { isRecurring }) });
}, { id: "hr.createHoliday" });

export const getLeaveReport = procedure(async () => {
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
}, { id: "hr.getLeaveReport" });

// ---------------------------------------------------------------------------
// Payroll (8 endpoints)
// ---------------------------------------------------------------------------

export const createPayrollRun = procedure(async (period: string, processedBy: number) => {
  return db.payrollRuns.insert({
    period,
    runDate: Date.now(),
    processedBy,
  });
}, { id: "hr.createPayrollRun" });

export const processPayroll = procedure(async (payrollRunId: number) => {
  await db.payrollRuns.update({ id: payrollRunId }, { status: "processing" });

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

    await db.payslips.insert({
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

  await db.payrollRuns.update(
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
}, { id: "hr.processPayroll" });

export const finalizePayroll = procedure(async (payrollRunId: number) => {
  await db.payslips.updateMany({ payrollRunId, status: "pending" }, { status: "paid" });
  return { data: { finalized: true }, error: null };
}, { id: "hr.finalizePayroll" });

export const getPayrollRuns = procedure(async (filters?: Record<string, unknown>) => {
  return db.payrollRuns.find(filters || {}).sort({ runDate: -1 });
}, { id: "hr.getPayrollRuns" });

export const getPayslip = procedure(async (id: number) => {
  return db.payslips.get({ id });
}, { id: "hr.getPayslip" });

export const getPayslipsByEmployee = procedure(async (employeeId: number) => {
  return db.payslips.find({ employeeId }).sort({ created_at: -1 });
}, { id: "hr.getPayslipsByEmployee" });

export const getPayrollSummary = procedure(async (period: string) => {
  const { data: run } = await db.payrollRuns.get({ period });
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
}, { id: "hr.getPayrollSummary" });

export const exportPayroll = procedure(async (payrollRunId: number) => {
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
}, { id: "hr.exportPayroll" });

// ---------------------------------------------------------------------------
// Performance (10 endpoints)
// ---------------------------------------------------------------------------

export const createReview = procedure(async (
  employeeId: number,
  reviewerId: number,
  period: string,
  cycle: string,
  strengths?: string,
  improvements?: string,
  goalsText?: string
) => {
  return db.reviews.insert({
    employeeId, reviewerId, period, cycle,
    ...(strengths    && { strengths }),
    ...(improvements && { improvements }),
    ...(goalsText    && { goals: goalsText }),
  });
}, { id: "hr.createReview" });

export const submitReview = procedure(async (id: number, rating: number) => {
  return db.reviews.update({ id }, { status: "submitted", rating });
}, { id: "hr.submitReview" });

export const acknowledgeReview = procedure(async (id: number) => {
  return db.reviews.update({ id }, { status: "acknowledged" });
}, { id: "hr.acknowledgeReview" });

export const getReviewsForEmployee = procedure(async (employeeId: number) => {
  return db.reviews.find({ employeeId }).sort({ created_at: -1 });
}, { id: "hr.getReviewsForEmployee" });

export const getPendingReviews = procedure(async (reviewerId?: number) => {
  return db.reviews.find({
    status: "draft",
    ...(reviewerId !== undefined && { reviewerId }),
  }).sort({ created_at: 1 });
}, { id: "hr.getPendingReviews" });

export const createGoal = procedure(async (
  employeeId: number,
  title: string,
  category: string,
  targetDate?: number,
  description?: string
) => {
  return db.goals.insert({
    employeeId, title, category,
    ...(targetDate  !== undefined && { targetDate }),
    ...(description && { description }),
  });
}, { id: "hr.createGoal" });

export const updateGoalProgress = procedure(async (id: number, progress: number, status?: string) => {
  return db.goals.update({ id }, {
    progress,
    ...(status && { status }),
  });
}, { id: "hr.updateGoalProgress" });

export const getGoals = procedure(async (employeeId: number, filters?: Record<string, unknown>) => {
  return db.goals.find({ employeeId, ...(filters || {}) }).sort({ targetDate: 1 });
}, { id: "hr.getGoals" });

export const giveFeedback = procedure(async (
  fromEmployeeId: number,
  toEmployeeId: number,
  type: string,
  message: string,
  isAnonymous?: boolean
) => {
  return db.feedback.insert({
    fromEmployeeId, toEmployeeId, type, message,
    ...(isAnonymous !== undefined && { isAnonymous }),
  });
}, { id: "hr.giveFeedback" });

export const getFeedbackForEmployee = procedure(async (toEmployeeId: number) => {
  return db.feedback.find({ toEmployeeId }).sort({ created_at: -1 });
}, { id: "hr.getFeedbackForEmployee" });

// ---------------------------------------------------------------------------
// Training (8 endpoints)
// ---------------------------------------------------------------------------

export const createCourse = procedure(async (
  title: string,
  category: string,
  durationHours: number,
  description?: string,
  isMandatory?: boolean,
  maxParticipants?: number
) => {
  return db.courses.insert({
    title, category, durationHours,
    ...(description    && { description }),
    ...(isMandatory    !== undefined && { isMandatory }),
    ...(maxParticipants !== undefined && { maxParticipants }),
  });
}, { id: "hr.createCourse" });

export const getCourses = procedure(async (filters?: Record<string, unknown>) => {
  return db.courses.find(filters || {}).sort({ title: 1 });
}, { id: "hr.getCourses" });

export const enrollInCourse = procedure(async (courseId: number, employeeId: number) => {
  return db.enrollments.insert({
    courseId, employeeId,
    enrolledAt: Date.now(),
  });
}, { id: "hr.enrollInCourse" });

export const completeCourse = procedure(async (
  courseId: number,
  employeeId: number,
  score?: number
) => {
  return db.enrollments.update(
    { courseId, employeeId },
    {
      status: "completed",
      completedAt: Date.now(),
      ...(score !== undefined && { score }),
    }
  );
}, { id: "hr.completeCourse" });

export const getEnrollments = procedure(async (filters?: Record<string, unknown>) => {
  return db.enrollments.find(filters || {}).sort({ enrolledAt: -1 });
}, { id: "hr.getEnrollments" });

export const addCertification = procedure(async (
  employeeId: number,
  name: string,
  issuer: string,
  issueDate: number,
  expiryDate?: number,
  credentialUrl?: string
) => {
  return db.certifications.insert({
    employeeId, name, issuer, issueDate,
    ...(expiryDate    !== undefined && { expiryDate }),
    ...(credentialUrl && { credentialUrl }),
  });
}, { id: "hr.addCertification" });

export const getCertifications = procedure(async (employeeId: number) => {
  return db.certifications.find({ employeeId }).sort({ issueDate: -1 });
}, { id: "hr.getCertifications" });

export const getExpiringCertifications = procedure(async (daysAhead: number = 30) => {
  const now    = Date.now();
  const cutoff = now + daysAhead * 86_400_000;
  return db.certifications.find({
    expiryDate: { $gte: now, $lte: cutoff },
  }).sort({ expiryDate: 1 });
}, { id: "hr.getExpiringCertifications" });

// ---------------------------------------------------------------------------
// Compensation & Benefits (8 endpoints)
// ---------------------------------------------------------------------------

export const adjustSalary = procedure(async (
  employeeId: number,
  newSalary: number,
  changeType: string,
  effectiveDate: number,
  changeReason?: string,
  approvedBy?: number
) => {
  await db.employees.update({ id: employeeId }, { salary: newSalary });

  return db.compensationHistory.insert({
    employeeId,
    effectiveDate,
    salary: newSalary,
    changeType,
    ...(changeReason && { changeReason }),
    ...(approvedBy !== undefined && { approvedBy }),
  });
}, { id: "hr.adjustSalary" });

export const getCompensationHistory = procedure(async (employeeId: number) => {
  return db.compensationHistory.find({ employeeId }).sort({ effectiveDate: -1 });
}, { id: "hr.getCompensationHistory" });

export const getBenefitsPlans = procedure(async (type?: string) => {
  return db.benefitsPlans.find(type ? { type } : {}).sort({ name: 1 });
}, { id: "hr.getBenefitsPlans" });

export const enrollInBenefit = procedure(async (
  employeeId: number,
  planId: number,
  startDate: number
) => {
  return db.benefitsEnrollments.insert({ employeeId, planId, startDate });
}, { id: "hr.enrollInBenefit" });

export const getBenefitsEnrollments = procedure(async (employeeId: number) => {
  return db.benefitsEnrollments.find({ employeeId, status: "active" }).sort({ startDate: -1 });
}, { id: "hr.getBenefitsEnrollments" });

export const submitExpense = procedure(async (
  employeeId: number,
  description: string,
  amount: number,
  category: string,
  receiptUrl?: string
) => {
  return db.expenseClaims.insert({
    employeeId, description, amount, category,
    submittedAt: Date.now(),
    ...(receiptUrl && { receiptUrl }),
  });
}, { id: "hr.submitExpense" });

export const approveExpense = procedure(async (id: number, approvedBy: number, approve: boolean) => {
  return db.expenseClaims.update(
    { id },
    { status: approve ? "approved" : "rejected", approvedBy }
  );
}, { id: "hr.approveExpense" });

export const getExpenses = procedure(async (filters?: Record<string, unknown>) => {
  return db.expenseClaims.find(filters || {}).sort({ submittedAt: -1 });
}, { id: "hr.getExpenses" });

// ---------------------------------------------------------------------------
// Documents & Compliance (6 endpoints)
// ---------------------------------------------------------------------------

export const uploadDocument = procedure(async (
  employeeId: number,
  type: string,
  name: string,
  fileUrl: string,
  expiresAt?: number
) => {
  return db.documents.insert({
    employeeId, type, name, fileUrl,
    uploadedAt: Date.now(),
    ...(expiresAt !== undefined && { expiresAt }),
  });
}, { id: "hr.uploadDocument" });

export const getDocuments = procedure(async (employeeId: number, type?: string) => {
  return db.documents.find({
    employeeId,
    ...(type && { type }),
  }).sort({ uploadedAt: -1 });
}, { id: "hr.getDocuments" });

export const getExpiringDocuments = procedure(async (daysAhead: number = 30) => {
  const now    = Date.now();
  const cutoff = now + daysAhead * 86_400_000;
  return db.documents.find({
    expiresAt: { $gte: now, $lte: cutoff },
  }).sort({ expiresAt: 1 });
}, { id: "hr.getExpiringDocuments" });

export const getPolicies = procedure(async (category?: string) => {
  return db.policies.find(category ? { category } : {}).sort({ effectiveDate: -1 });
}, { id: "hr.getPolicies" });

export const getAuditLog = procedure(async (
  entityType?: string,
  entityId?: number,
  actorId?: number
) => {
  return db.auditLog.find({
    ...(entityType !== undefined && { entityType }),
    ...(entityId   !== undefined && { entityId }),
    ...(actorId    !== undefined && { actorId }),
  }).sort({ timestamp: -1 }).limit(500);
}, { id: "hr.getAuditLog" });

export const acknowledgePolicy = procedure(async (employeeId: number, policyId: number) => {
  // Record acknowledgement as a document
  return db.documents.insert({
    employeeId,
    type: "policy",
    name: `Policy ${policyId} Acknowledgement`,
    fileUrl: "",
    uploadedAt: Date.now(),
  });
}, { id: "hr.acknowledgePolicy" });

// ---------------------------------------------------------------------------
// Notifications (4 endpoints)
// ---------------------------------------------------------------------------

export const getNotifications = procedure(async (employeeId: number) => {
  return db.notifications.find({ employeeId }).sort({ created_at: -1 });
}, { id: "hr.getNotifications" });

export const markAsRead = procedure(async (id: number) => {
  return db.notifications.update({ id }, { isRead: true });
}, { id: "hr.markAsRead" });

export const markAllAsRead = procedure(async (employeeId: number) => {
  return db.notifications.updateMany({ employeeId, isRead: false }, { isRead: true });
}, { id: "hr.markAllAsRead" });

export const getUnreadCount = procedure(async (employeeId: number) => {
  return db.notifications.count({ employeeId, isRead: false });
}, { id: "hr.getUnreadCount" });

// ---------------------------------------------------------------------------
// Analytics (10 endpoints)
// ---------------------------------------------------------------------------

export const getDashboard = procedure(async () => {
  const [
    { data: totalEmployees },
    { data: totalDepartments },
    { data: openPositions },
    { data: pendingLeaves },
    { data: openJobs },
  ] = await Promise.all([
    db.employees.count({ status: "active" }),
    db.departments.count({}),
    db.positions.count({ isOpen: true }),
    db.leaveRequests.count({ status: "pending" }),
    db.jobPostings.count({ status: "open" }),
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
}, { id: "hr.getDashboard" });

export const getHeadcountByDepartment = procedure(async () => {
  return db.employees.aggregate([
    { $match: { status: "active" } },
    { $group: { _id: "$departmentId", headcount: { $sum: 1 }, avgSalary: { $avg: "$salary" } } },
    { $sort: { headcount: -1 } },
  ]);
}, { id: "hr.getHeadcountByDepartment" });

export const getSalaryDistribution = procedure(async () => {
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
}, { id: "hr.getSalaryDistribution" });

export const getAttritionReport = procedure(async () => {
  const { data: terminated } = await db.employees.count({ status: "terminated" });
  const { data: total }      = await db.employees.count({});
  const rate = total ? (((terminated as number) || 0) / (total as number) * 100).toFixed(1) : "0.0";
  return { data: { terminated, total, attritionRate: `${rate}%` }, error: null };
}, { id: "hr.getAttritionReport" });

export const getLeaveUtilization = procedure(async () => {
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
}, { id: "hr.getLeaveUtilization" });

export const getReviewStats = procedure(async () => {
  return db.reviews.aggregate([
    { $group: { _id: "$period", count: { $sum: 1 }, avgRating: { $avg: "$rating" } } },
    { $sort: { _id: -1 } },
  ]);
}, { id: "hr.getReviewStats" });

export const getTimeToHire = procedure(async () => {
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
}, { id: "hr.getTimeToHire" });

export const getDiversityMetrics = procedure(async () => {
  // Department composition with skill breakdown
  return db.employees.aggregate([
    { $match: { status: "active" } },
    { $group: { _id: "$departmentId", headcount: { $sum: 1 } } },
    { $sort: { headcount: -1 } },
  ]);
}, { id: "hr.getDiversityMetrics" });

export const getCostPerEmployee = procedure(async () => {
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
}, { id: "hr.getCostPerEmployee" });

export const getMonthlyTrends = procedure(async () => {
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
}, { id: "hr.getMonthlyTrends" });
