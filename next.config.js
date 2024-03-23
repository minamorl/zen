/** @type {import('next').NextConfig} */
const nextConfig = {
  env: {
    S3_UPLOAD_KEY: process.env.S3_UPLOAD_KEY,
    S3_UPLOAD_SECRET: process.env.S3_UPLOAD_SECRET,
    S3_UPLOAD_BUCKET: process.env.S3_UPLOAD_BUCKET,
    S3_UPLOAD_ENDPOINT: process.env.S3_UPLOAD_ENDPOINT,
    S3_UPLOAD_PORT: process.env.S3_UPLOAD_PORT,
    S3_UPLOAD_REGION: process.env.S3_UPLOAD_REGION,
  },
};

module.exports = nextConfig;
