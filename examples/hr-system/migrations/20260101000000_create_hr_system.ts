import { table, t } from "@zeroship/migrate";

// hr-system's schema, authored migration-first.
//
// WHY THIS FILE EXISTS. This example declared its schema INLINE
// (`export default { schema: dbSchema }`) with no `migrations/`, which is the
// #209/#174 mechanism: `env.db` is installed from the generated runtime
// descriptor, folded from COMMITTED MIGRATIONS, and an inline `schema` export
// is not a source for it. Measured 2026-08-12 by booting its dev server and
// calling `hr.getEmployees`: `500`, with the server log giving
// `TypeError: Cannot read properties of undefined (reading 'find')`, and
// `.zeroship/` holding no `dev.sqlite` at all.
//
// TWO SPELLING RULES, both learned by failing rather than from the types:
//
//   1. The seven platform system columns (id, created_at, updated_at,
//      created_by, updated_by, version, deleted_at) are INJECTED by the
//      confined charter. Declaring `id` here collides with the injected column
//      and the descriptor is refused.
//
//   2. `t.ref()` is DECLARED in @zeroship/migrate's public types but is not
//      accepted by the vendored engine. It does not arise here: hr-system's
//      schema contains ZERO `t.ref()` - its cross-table links are plain
//      `t.number()` id columns (`managerId`, `departmentId`, ...), so this
//      migration declares no foreign keys and none are lost in translation.
//
// THE TRANSLATION IS MECHANICAL, four column factories and nothing else. The
// source uses only `t.string` (81), `t.number` (101), `t.boolean` (6) and one
// `t.array`; it contains no `t.encrypted`, `t.vector`, `t.geoPoint`, `.fts` or
// `.mask`, so unlike examples/db-e2e nothing here touches the IR gap that
// blocks a non-default encrypted column.
//
//     t.string()  -> t.text()        t.boolean() -> t.boolean()
//     t.number()  -> t.double()      t.array(..) -> t.json()
//     .required() -> .notNull()
//     .default(x) -> .notNull().default(x)   (a defaulted column is never null
//                    in practice; same shape examples/db-todos uses for
//                    `done: t.boolean().notNull().default(false)`)
//     .enum(..) / .min(..) / .max(..) are VALIDATION, not storage, and do not
//                    appear here - the column stays its base type.
//
// 27 tables, 189 columns. Both numbers are from parsing the schema block, not
// from grepping: an earlier unscoped grep of mine said "32 tables, 186 columns"
// and both halves were wrong - it counted five `data: {` object literals inside
// RPC handler bodies as tables, and its `:\s+t\.` pattern missed three columns
// written with no space after the colon.
export default {
  name: "create_hr_system",
  up() {
    table("departments").create({
      columns: {
        name: t.text().notNull(),
        code: t.text().notNull(),
        managerId: t.double(),
        budget: t.double().notNull().default(0),
        headcount: t.double().notNull().default(0),
        parentDepartmentId: t.double(),
      },
    });

    table("positions").create({
      columns: {
        title: t.text().notNull(),
        departmentId: t.double().notNull(),
        level: t.text(),
        salaryMin: t.double(),
        salaryMax: t.double(),
        isOpen: t.boolean().notNull().default(true),
        description: t.text(),
      },
    });

    table("employees").create({
      columns: {
        firstName: t.text().notNull(),
        lastName: t.text().notNull(),
        email: t.text().notNull(),
        phone: t.text(),
        departmentId: t.double(),
        positionId: t.double(),
        managerId: t.double(),
        hireDate: t.double(),
        salary: t.double(),
        status: t.text().notNull().default("active"),
        skills: t.json(),
        avatarUrl: t.text(),
        emergencyContactName: t.text(),
        emergencyContactPhone: t.text(),
        address: t.text(),
        dateOfBirth: t.double(),
      },
    });

    table("jobPostings").create({
      columns: {
        positionId: t.double().notNull(),
        title: t.text().notNull(),
        description: t.text(),
        requirements: t.text(),
        status: t.text().notNull().default("draft"),
        postedDate: t.double(),
        closingDate: t.double(),
      },
    });

    table("applicants").create({
      columns: {
        jobPostingId: t.double().notNull(),
        name: t.text().notNull(),
        email: t.text().notNull(),
        phone: t.text(),
        resumeUrl: t.text(),
        stage: t.text().notNull().default("applied"),
        rating: t.double(),
        notes: t.text(),
        appliedDate: t.double(),
      },
    });

    table("interviews").create({
      columns: {
        applicantId: t.double().notNull(),
        interviewerId: t.double().notNull(),
        scheduledAt: t.double().notNull(),
        durationMinutes: t.double().notNull().default(60),
        type: t.text().notNull().default("video"),
        status: t.text().notNull().default("scheduled"),
        feedback: t.text(),
        rating: t.double(),
      },
    });

    table("timesheets").create({
      columns: {
        employeeId: t.double().notNull(),
        date: t.double().notNull(),
        clockIn: t.double(),
        clockOut: t.double(),
        hoursWorked: t.double().notNull().default(0),
        overtimeHours: t.double().notNull().default(0),
        status: t.text().notNull().default("draft"),
        notes: t.text(),
      },
    });

    table("workSchedules").create({
      columns: {
        employeeId: t.double().notNull(),
        dayOfWeek: t.double().notNull(),
        startTime: t.text().notNull(),
        endTime: t.text().notNull(),
        isRemote: t.boolean().notNull().default(false),
      },
    });

    table("leaveRequests").create({
      columns: {
        employeeId: t.double().notNull(),
        type: t.text().notNull(),
        startDate: t.double().notNull(),
        endDate: t.double().notNull(),
        days: t.double().notNull(),
        reason: t.text(),
        status: t.text().notNull().default("pending"),
        approvedBy: t.double(),
        approvedAt: t.double(),
      },
    });

    table("leaveBalances").create({
      columns: {
        employeeId: t.double().notNull(),
        year: t.double().notNull(),
        vacationTotal: t.double().notNull().default(20),
        vacationUsed: t.double().notNull().default(0),
        sickTotal: t.double().notNull().default(10),
        sickUsed: t.double().notNull().default(0),
        personalTotal: t.double().notNull().default(5),
        personalUsed: t.double().notNull().default(0),
      },
    });

    table("holidays").create({
      columns: {
        name: t.text().notNull(),
        date: t.double().notNull(),
        isRecurring: t.boolean().notNull().default(false),
      },
    });

    table("payrollRuns").create({
      columns: {
        period: t.text().notNull(),
        runDate: t.double(),
        status: t.text().notNull().default("draft"),
        totalGross: t.double().notNull().default(0),
        totalNet: t.double().notNull().default(0),
        totalDeductions: t.double().notNull().default(0),
        processedBy: t.double(),
      },
    });

    table("payslips").create({
      columns: {
        payrollRunId: t.double().notNull(),
        employeeId: t.double().notNull(),
        baseSalary: t.double().notNull(),
        overtimePay: t.double().notNull().default(0),
        bonus: t.double().notNull().default(0),
        deductionsTax: t.double().notNull().default(0),
        deductionsBenefits: t.double().notNull().default(0),
        deductionsOther: t.double().notNull().default(0),
        netPay: t.double().notNull(),
        status: t.text().notNull().default("pending"),
      },
    });

    table("reviews").create({
      columns: {
        employeeId: t.double().notNull(),
        reviewerId: t.double().notNull(),
        period: t.text().notNull(),
        cycle: t.text().notNull().default("annual"),
        rating: t.double(),
        strengths: t.text(),
        improvements: t.text(),
        goals: t.text(),
        status: t.text().notNull().default("draft"),
      },
    });

    table("goals").create({
      columns: {
        employeeId: t.double().notNull(),
        title: t.text().notNull(),
        description: t.text(),
        targetDate: t.double(),
        status: t.text().notNull().default("active"),
        progress: t.double().notNull().default(0),
        category: t.text().notNull().default("performance"),
      },
    });

    table("feedback").create({
      columns: {
        fromEmployeeId: t.double().notNull(),
        toEmployeeId: t.double().notNull(),
        type: t.text().notNull(),
        message: t.text().notNull(),
        isAnonymous: t.boolean().notNull().default(false),
      },
    });

    table("courses").create({
      columns: {
        title: t.text().notNull(),
        description: t.text(),
        category: t.text(),
        durationHours: t.double(),
        isMandatory: t.boolean().notNull().default(false),
        maxParticipants: t.double(),
      },
    });

    table("enrollments").create({
      columns: {
        courseId: t.double().notNull(),
        employeeId: t.double().notNull(),
        status: t.text().notNull().default("enrolled"),
        enrolledAt: t.double(),
        completedAt: t.double(),
        score: t.double(),
      },
    });

    table("certifications").create({
      columns: {
        employeeId: t.double().notNull(),
        name: t.text().notNull(),
        issuer: t.text(),
        issueDate: t.double(),
        expiryDate: t.double(),
        credentialUrl: t.text(),
      },
    });

    table("compensationHistory").create({
      columns: {
        employeeId: t.double().notNull(),
        effectiveDate: t.double().notNull(),
        salary: t.double().notNull(),
        changeType: t.text().notNull(),
        changeReason: t.text(),
        approvedBy: t.double(),
      },
    });

    table("benefitsPlans").create({
      columns: {
        name: t.text().notNull(),
        type: t.text().notNull(),
        provider: t.text(),
        monthlyCostEmployee: t.double().notNull().default(0),
        monthlyCostEmployer: t.double().notNull().default(0),
      },
    });

    table("benefitsEnrollments").create({
      columns: {
        employeeId: t.double().notNull(),
        planId: t.double().notNull(),
        startDate: t.double().notNull(),
        endDate: t.double(),
        status: t.text().notNull().default("active"),
      },
    });

    table("expenseClaims").create({
      columns: {
        employeeId: t.double().notNull(),
        description: t.text().notNull(),
        amount: t.double().notNull(),
        category: t.text().notNull(),
        receiptUrl: t.text(),
        status: t.text().notNull().default("submitted"),
        submittedAt: t.double(),
        approvedBy: t.double(),
      },
    });

    table("documents").create({
      columns: {
        employeeId: t.double().notNull(),
        type: t.text().notNull(),
        name: t.text().notNull(),
        fileUrl: t.text().notNull(),
        uploadedAt: t.double(),
        expiresAt: t.double(),
      },
    });

    table("auditLog").create({
      columns: {
        actorId: t.double().notNull(),
        action: t.text().notNull(),
        entityType: t.text().notNull(),
        entityId: t.double().notNull(),
        changesJson: t.text(),
        timestamp: t.double(),
      },
    });

    table("policies").create({
      columns: {
        title: t.text().notNull(),
        content: t.text(),
        policyVersion: t.text(),
        effectiveDate: t.double(),
        category: t.text().notNull(),
      },
    });

    table("notifications").create({
      columns: {
        employeeId: t.double().notNull(),
        type: t.text().notNull(),
        title: t.text().notNull(),
        message: t.text(),
        isRead: t.boolean().notNull().default(false),
        link: t.text(),
      },
    });
  },
};
