import { defineApp } from "@zeroship/server";
export default defineApp({ resources: {
  "rpc:gather.session": {
    "auth": "anonymous",
    "publiclyAccessible": true,
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.catalog": {
    "auth": "anonymous",
    "publiclyAccessible": true,
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.recipe": {
    "auth": "anonymous",
    "publiclyAccessible": true,
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.waitlist": {
    "auth": "anonymous",
    "publiclyAccessible": true,
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.quote": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.checkout": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.account": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.order": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.pay": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.cancelOrder": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.editOrder": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.plan": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.renewalPreview": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.renewal": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.issue": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.receipt": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.saveAddress": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.deleteAddress": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.preferences": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.requestPrivacy": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.cancelPrivacy": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.privacyExport": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.cookingRecipe": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "rpc:gather.saveRecipeFeedback": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192
  },
  "/api/draft-session": {
    "auth": "anonymous",
    "publiclyAccessible": true,
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192,
    "rateLimit": {
      "rpm": 240,
      "per": "ip"
    }
  },
  "/api/drafts/load": {
    "auth": "anonymous",
    "publiclyAccessible": true,
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192,
    "rateLimit": {
      "rpm": 240,
      "per": "ip"
    }
  },
  "/api/drafts/save": {
    "auth": "anonymous",
    "publiclyAccessible": true,
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192,
    "rateLimit": {
      "rpm": 240,
      "per": "ip"
    }
  },
  "/api/drafts/attach": {
    "auth": "user",
    "cache": {
      "maxAge": 0
    },
    "maxInputBytes": 8192,
    "rateLimit": {
      "rpm": 240,
      "per": "ip"
    }
  }
} });
