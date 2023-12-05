/*
  Warnings:

  - You are about to drop the column `email` on the `Key` table. All the data in the column will be lost.

*/
-- AlterTable
ALTER TABLE "Key" DROP COLUMN "email";

-- AlterTable
ALTER TABLE "User" ADD COLUMN     "email" TEXT;
