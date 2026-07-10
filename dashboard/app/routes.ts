import {
  type RouteConfig,
  index,
  layout,
  route,
} from "@react-router/dev/routes";

export default [
  layout("routes/layout.tsx", [
    index("routes/index.tsx"),
    route("sessions/:id", "routes/session.tsx"),
    route("origins/:host", "routes/origin.tsx"),
  ]),
] satisfies RouteConfig;
