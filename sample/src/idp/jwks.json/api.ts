export default {
  get() {
    json.raw(oidc.jwks());
  },
};
