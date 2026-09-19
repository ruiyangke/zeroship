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
//      `t.double()` id columns (`managerId`, `departmentId`, ...), so this
//      migration declares no foreign keys and none are lost in translation.
//
// THE TRANSLATION IS MECHANICAL, four column factories and nothing else. The
// source uses only `t.string` (81), `t.number` (101), `t.boolean` (6) and one
// `t.array`; it contains no `.encrypted()`, `t.vector`, `t.geoPoint` or
// `.mask`, so unlike examples/db-e2e nothing here touches the IR gap that
// blocks a non-default encrypted column.
//
//     t.string()  -> t.text()        t.boolean() -> t.boolean()
//     t.double()  -> t.double()      t.array(..) -> t.json()
//     .required() -> .required()
//     .default(x) -> .required().default(x)   (a defaulted column is never null
//                    in practice; same shape examples/db-todos uses for
//                    `done: t.boolean().required().default(false)`)
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
  schema() {
    table("departments").create({
      columns: {
        name: t.text().required(),
        code: t.text().required(),
        managerId: t.double(),
        budget: t.double().required().default(0),
        headcount: t.double().required().default(0),
        parentDepartmentId: t.double(),
      },
    });

    table("positions").create({
      columns: {
        title: t.text().required(),
        departmentId: t.double().required(),
        level: t.text(),
        salaryMin: t.double(),
        salaryMax: t.double(),
        isOpen: t.boolean().required().default(true),
        description: t.text(),
      },
    });

    table("employees").create({
      columns: {
        firstName: t.text().required(),
        lastName: t.text().required(),
        email: t.text().required(),
        phone: t.text(),
        departmentId: t.double(),
        positionId: t.double(),
        managerId: t.double(),
        hireDate: t.double(),
        salary: t.double(),
        status: t.text().required().default("active"),
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
        positionId: t.double().required(),
        title: t.text().required(),
        description: t.text(),
        requirements: t.text(),
        status: t.text().required().default("draft"),
        postedDate: t.double(),
        closingDate: t.double(),
      },
    });

    table("applicants").create({
      columns: {
        jobPostingId: t.double().required(),
        name: t.text().required(),
        email: t.text().required(),
        phone: t.text(),
        resumeUrl: t.text(),
        stage: t.text().required().default("applied"),
        rating: t.double(),
        notes: t.text(),
        appliedDate: t.double(),
      },
    });

    table("interviews").create({
      columns: {
        applicantId: t.double().required(),
        interviewerId: t.double().required(),
        scheduledAt: t.double().required(),
        durationMinutes: t.double().required().default(60),
        type: t.text().required().default("video"),
        status: t.text().required().default("scheduled"),
        feedback: t.text(),
        rating: t.double(),
      },
    });

    table("timesheets").create({
      columns: {
        employeeId: t.double().required(),
        date: t.double().required(),
        clockIn: t.double(),
        clockOut: t.double(),
        hoursWorked: t.double().required().default(0),
        overtimeHours: t.double().required().default(0),
        status: t.text().required().default("draft"),
        notes: t.text(),
      },
    });

    table("workSchedules").create({
      columns: {
        employeeId: t.double().required(),
        dayOfWeek: t.double().required(),
        startTime: t.text().required(),
        endTime: t.text().required(),
        isRemote: t.boolean().required().default(false),
      },
    });

    table("leaveRequests").create({
      columns: {
        employeeId: t.double().required(),
        type: t.text().required(),
        startDate: t.double().required(),
        endDate: t.double().required(),
        days: t.double().required(),
        reason: t.text(),
        status: t.text().required().default("pending"),
        approvedBy: t.double(),
        approvedAt: t.double(),
      },
    });

    table("leaveBalances").create({
      columns: {
        employeeId: t.double().required(),
        year: t.double().required(),
        vacationTotal: t.double().required().default(20),
        vacationUsed: t.double().required().default(0),
        sickTotal: t.double().required().default(10),
        sickUsed: t.double().required().default(0),
        personalTotal: t.double().required().default(5),
        personalUsed: t.double().required().default(0),
      },
    });

    table("holidays").create({
      columns: {
        name: t.text().required(),
        date: t.double().required(),
        isRecurring: t.boolean().required().default(false),
      },
    });

    table("payrollRuns").create({
      columns: {
        period: t.text().required(),
        runDate: t.double(),
        status: t.text().required().default("draft"),
        totalGross: t.double().required().default(0),
        totalNet: t.double().required().default(0),
        totalDeductions: t.double().required().default(0),
        processedBy: t.double(),
      },
    });

    table("payslips").create({
      columns: {
        payrollRunId: t.double().required(),
        employeeId: t.double().required(),
        baseSalary: t.double().required(),
        overtimePay: t.double().required().default(0),
        bonus: t.double().required().default(0),
        deductionsTax: t.double().required().default(0),
        deductionsBenefits: t.double().required().default(0),
        deductionsOther: t.double().required().default(0),
        netPay: t.double().required(),
        status: t.text().required().default("pending"),
      },
    });

    table("reviews").create({
      columns: {
        employeeId: t.double().required(),
        reviewerId: t.double().required(),
        period: t.text().required(),
        cycle: t.text().required().default("annual"),
        rating: t.double(),
        strengths: t.text(),
        improvements: t.text(),
        goals: t.text(),
        status: t.text().required().default("draft"),
      },
    });

    table("goals").create({
      columns: {
        employeeId: t.double().required(),
        title: t.text().required(),
        description: t.text(),
        targetDate: t.double(),
        status: t.text().required().default("active"),
        progress: t.double().required().default(0),
        category: t.text().required().default("performance"),
      },
    });

    table("feedback").create({
      columns: {
        fromEmployeeId: t.double().required(),
        toEmployeeId: t.double().required(),
        type: t.text().required(),
        message: t.text().required(),
        isAnonymous: t.boolean().required().default(false),
      },
    });

    table("courses").create({
      columns: {
        title: t.text().required(),
        description: t.text(),
        category: t.text(),
        durationHours: t.double(),
        isMandatory: t.boolean().required().default(false),
        maxParticipants: t.double(),
      },
    });

    table("enrollments").create({
      columns: {
        courseId: t.double().required(),
        employeeId: t.double().required(),
        status: t.text().required().default("enrolled"),
        enrolledAt: t.double(),
        completedAt: t.double(),
        score: t.double(),
      },
    });

    table("certifications").create({
      columns: {
        employeeId: t.double().required(),
        name: t.text().required(),
        issuer: t.text(),
        issueDate: t.double(),
        expiryDate: t.double(),
        credentialUrl: t.text(),
      },
    });

    table("compensationHistory").create({
      columns: {
        employeeId: t.double().required(),
        effectiveDate: t.double().required(),
        salary: t.double().required(),
        changeType: t.text().required(),
        changeReason: t.text(),
        approvedBy: t.double(),
      },
    });

    table("benefitsPlans").create({
      columns: {
        name: t.text().required(),
        type: t.text().required(),
        provider: t.text(),
        monthlyCostEmployee: t.double().required().default(0),
        monthlyCostEmployer: t.double().required().default(0),
      },
    });

    table("benefitsEnrollments").create({
      columns: {
        employeeId: t.double().required(),
        planId: t.double().required(),
        startDate: t.double().required(),
        endDate: t.double(),
        status: t.text().required().default("active"),
      },
    });

    table("expenseClaims").create({
      columns: {
        employeeId: t.double().required(),
        description: t.text().required(),
        amount: t.double().required(),
        category: t.text().required(),
        receiptUrl: t.text(),
        status: t.text().required().default("submitted"),
        submittedAt: t.double(),
        approvedBy: t.double(),
      },
    });

    table("documents").create({
      columns: {
        employeeId: t.double().required(),
        type: t.text().required(),
        name: t.text().required(),
        fileUrl: t.text().required(),
        uploadedAt: t.double(),
        expiresAt: t.double(),
      },
    });

    table("auditLog").create({
      columns: {
        actorId: t.double().required(),
        action: t.text().required(),
        entityType: t.text().required(),
        entityId: t.double().required(),
        changesJson: t.text(),
        timestamp: t.double(),
      },
    });

    table("policies").create({
      columns: {
        title: t.text().required(),
        content: t.text(),
        policyVersion: t.text(),
        effectiveDate: t.double(),
        category: t.text().required(),
      },
    });

    table("notifications").create({
      columns: {
        employeeId: t.double().required(),
        type: t.text().required(),
        title: t.text().required(),
        message: t.text(),
        isRead: t.boolean().required().default(false),
        link: t.text(),
      },
    });
  },
};
